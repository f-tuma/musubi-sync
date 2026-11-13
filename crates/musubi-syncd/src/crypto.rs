//! Caller tokens, sealed account tokens and the [`TokenVault`] over them.
//!
//! Everything uses `ring`, which is in the tree anyway as the rustls provider.
//!
//! **Short-lived token** (`POST /v1/tokens`):
//! `mt1.<kid>.<payload>.<signature>`. `kid` is two hex digits naming the
//! signing key (first byte of SHA-256 of its public key); payload and
//! signature are base64url without padding. The payload is JSON
//! `{"tenant","scope","iat","exp"}` (Unix seconds); the signature is Ed25519
//! over the ASCII bytes `mt1.<kid>.<payload>`. Verification needs only the
//! public key and the clock, no database. A new version gets a new prefix.
//! Rotating the signing key invalidates every outstanding token (at most
//! 15 minutes old); the `kid` lets a later version accept the previous key too.
//!
//! **Sealed account tokens** ([`SealedTokens`]): `0x01 | kid (1) | nonce (12) |
//! AES-256-GCM ciphertext and tag` over the JSON `{"refresh","access","until"}`.
//! `kid` names the token key (first byte of SHA-256 of a domain tag and the
//! key) for rotation (proposal ch. 7). The associated data is
//! `musubi-tokens-v1 | 0x01 | kid | tenant (16) | account (16)`, so a blob
//! copied to another account or tenant row fails to open.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use musubi_model::control::{AccountTokens, Credentials, Scope, SealedTokens};
use musubi_model::{AccountId, ControlStore, StoreError, TenantId, TokenVault};
use musubi_store_sqlite::SqliteControl;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const TOKEN_PREFIX: &str = "mt1.";
const SEAL_VERSION: u8 = 1;
const SEAL_AAD: &[u8] = b"musubi-tokens-v1";
/// Version, key id and nonce.
const HEAD: usize = 2 + NONCE_LEN;

/// `n` random bytes from the OS.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    // shortcut: SystemRandom fails only without an OS RNG, where nothing can run safely anyway.
    SystemRandom::new()
        .fill(&mut b)
        .expect("OS random generator");
    b
}

/// 32 random bytes as base64url: API keys, OAuth `state`, PKCE verifier.
pub(crate) fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random::<32>())
}

pub(crate) fn sha256(b: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, b);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// Equality in time independent of where the inputs differ.
pub(crate) fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    std::hint::black_box(a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y))) == 0
}

pub(crate) fn b64(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

pub(crate) fn unb64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).ok()
}

/// Claims of a short-lived token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Tenant the token acts for.
    pub tenant: TenantId,
    /// Granted scope, at most the issuing key's.
    pub scope: Scope,
    /// Issued at, Unix seconds.
    pub iat: i64,
    /// Expires at, Unix seconds; rejected from this second on.
    pub exp: i64,
}

/// Issues and verifies short-lived tokens with one Ed25519 key.
pub(crate) struct Signer {
    pair: Ed25519KeyPair,
    /// `mt1.<kid>.`
    head: String,
}

impl Signer {
    pub(crate) fn new(seed: &[u8; 32]) -> Result<Self, String> {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed).map_err(|e| e.to_string())?;
        let kid = sha256(pair.public_key().as_ref())[0];
        Ok(Self {
            pair,
            head: format!("{TOKEN_PREFIX}{kid:02x}."),
        })
    }

    pub(crate) fn issue(&self, c: &Claims) -> String {
        // Serialising plain ids, an enum and integers cannot fail.
        let payload = b64(&serde_json::to_vec(c).expect("claims serialise"));
        let signed = format!("{}{payload}", self.head);
        let sig = self.pair.sign(signed.as_bytes());
        format!("{signed}.{}", b64(sig.as_ref()))
    }

    /// The claims of a token signed by this key and not expired at `now`.
    pub(crate) fn verify(&self, token: &str, now: Timestamp) -> Option<Claims> {
        let (signed, sig) = token.rsplit_once('.')?;
        let payload = signed.strip_prefix(&self.head)?;
        UnparsedPublicKey::new(&ED25519, self.pair.public_key().as_ref())
            .verify(signed.as_bytes(), &unb64(sig)?)
            .ok()?;
        let c: Claims = serde_json::from_slice(&unb64(payload)?).ok()?;
        (now.as_second() < c.exp).then_some(c)
    }
}

/// Seals and opens account tokens (AES-256-GCM bound to tenant and account).
pub(crate) struct Sealer {
    key: LessSafeKey,
    kid: u8,
}

#[derive(Serialize, Deserialize)]
struct Plain<S> {
    refresh: S,
    access: S,
    until: Timestamp,
}

fn aad(kid: u8, tenant: TenantId, account: AccountId) -> Vec<u8> {
    [
        SEAL_AAD,
        &[SEAL_VERSION, kid],
        &tenant.to_bytes(),
        &account.to_bytes(),
    ]
    .concat()
}

impl Sealer {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        let kid = sha256(&[b"musubi-token-key".as_slice(), key].concat())[0];
        let key = UnboundKey::new(&AES_256_GCM, key).expect("32-byte AES-256 key");
        Self {
            key: LessSafeKey::new(key),
            kid,
        }
    }

    pub(crate) fn seal(
        &self,
        tenant: TenantId,
        account: AccountId,
        t: &AccountTokens,
    ) -> SealedTokens {
        let plain = Plain {
            refresh: t.refresh.expose_secret(),
            access: t.access.expose_secret(),
            until: t.access_until,
        };
        let mut buf = Zeroizing::new(serde_json::to_vec(&plain).expect("tokens serialise"));
        let nonce = random::<NONCE_LEN>();
        let mut out = Vec::with_capacity(HEAD + buf.len() + AES_256_GCM.tag_len());
        out.extend_from_slice(&[SEAL_VERSION, self.kid]);
        out.extend_from_slice(&nonce);
        let tag = self
            .key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad(self.kid, tenant, account)),
                &mut buf[..],
            )
            .expect("AES-GCM input below its size limit");
        out.extend_from_slice(&buf);
        out.extend_from_slice(tag.as_ref());
        SealedTokens(out)
    }

    pub(crate) fn open(
        &self,
        tenant: TenantId,
        account: AccountId,
        sealed: &SealedTokens,
    ) -> Result<AccountTokens, StoreError> {
        let bad = || StoreError::Corrupt("sealed tokens do not open for this account".into());
        let b = &sealed.0;
        if b.len() < HEAD || b[0] != SEAL_VERSION {
            return Err(bad());
        }
        if b[1] != self.kid {
            return Err(StoreError::Corrupt(
                "sealed tokens use another token key".into(),
            ));
        }
        let nonce = Nonce::try_assume_unique_for_key(&b[2..HEAD]).map_err(|_| bad())?;
        let mut buf = Zeroizing::new(b[HEAD..].to_vec());
        let plain = self
            .key
            .open_in_place(
                nonce,
                Aad::from(aad(self.kid, tenant, account)),
                &mut buf[..],
            )
            .map_err(|_| bad())?;
        let p: Plain<String> = serde_json::from_slice(plain).map_err(|_| bad())?;
        Ok(AccountTokens {
            refresh: SecretString::from(p.refresh),
            access: SecretString::from(p.access),
            access_until: p.until,
        })
    }
}

/// [`TokenVault`] over [`ControlStore::write_tokens`] / [`ControlStore::read_tokens`].
pub struct Vault {
    pub(crate) control: Arc<SqliteControl>,
    pub(crate) sealer: Arc<Sealer>,
}

impl TokenVault for Vault {
    fn load(&self, account: AccountId) -> Result<Option<Credentials>, StoreError> {
        let Some(a) = self.control.account(account)? else {
            return Ok(None);
        };
        match self.control.read_tokens(account)? {
            Some(sealed) => self
                .sealer
                .open(a.tenant, account, &sealed)
                .map(|t| Some(Credentials::OAuth(t))),
            None => Ok(None),
        }
    }

    fn store(&self, account: AccountId, c: &Credentials) -> Result<(), StoreError> {
        // shortcut: OAuth only (phase 1 format); K1 adds Basic under `musubi-basic-v1`.
        let Credentials::OAuth(t) = c else {
            return Err(StoreError::Corrupt(
                "this vault stores OAuth tokens only".into(),
            ));
        };
        let a = self.control.account(account)?.ok_or(StoreError::NotFound)?;
        self.control
            .write_tokens(account, &self.sealer.seal(a.tenant, account, t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> AccountTokens {
        AccountTokens {
            refresh: "r".to_owned().into(),
            access: "a".to_owned().into(),
            access_until: Timestamp::UNIX_EPOCH,
        }
    }

    #[test]
    fn sealed_tokens_open_only_for_their_tenant_and_account() {
        let s = Sealer::new(&[7; 32]);
        let (t, a) = (TenantId::from_u128(1), AccountId::from_u128(2));
        let sealed = s.seal(t, a, &tokens());
        let back = s.open(t, a, &sealed).unwrap();
        assert_eq!(back.refresh.expose_secret(), "r");
        assert_eq!(back.access.expose_secret(), "a");
        assert!(s.open(t, AccountId::from_u128(3), &sealed).is_err());
        assert!(s.open(TenantId::from_u128(9), a, &sealed).is_err());
        assert!(
            Sealer::new(&[8; 32]).open(t, a, &sealed).is_err(),
            "other key"
        );
        let mut flipped = sealed.clone();
        *flipped.0.last_mut().unwrap() ^= 1;
        assert!(s.open(t, a, &flipped).is_err(), "tampered");
    }

    #[test]
    fn tokens_verify_only_with_the_key_and_before_expiry() {
        let s = Signer::new(&[1; 32]).unwrap();
        let c = Claims {
            tenant: TenantId::from_u128(5),
            scope: Scope::Read,
            iat: 100,
            exp: 200,
        };
        let tok = s.issue(&c);
        assert!(tok.starts_with("mt1.") && tok.split('.').count() == 4);
        let at = |s: i64| Timestamp::from_second(s).unwrap();
        assert_eq!(s.verify(&tok, at(199)), Some(c));
        assert_eq!(s.verify(&tok, at(200)), None, "expired");
        assert_eq!(Signer::new(&[2; 32]).unwrap().verify(&tok, at(150)), None);
        // A payload swapped for another tenant's breaks the signature.
        let other = s.issue(&Claims {
            tenant: TenantId::from_u128(6),
            ..c
        });
        let part = |t: &str, i| t.split('.').nth(i).unwrap().to_owned();
        let forged = format!(
            "mt1.{}.{}.{}",
            part(&tok, 1),
            part(&other, 2),
            part(&tok, 3)
        );
        assert_eq!(s.verify(&forged, at(150)), None);
    }
}
