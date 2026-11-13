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
//! copied to another account or tenant row fails to open. Basic credentials
//! (DAV, iCloud) use the same layout over `{"username","password"}` with the
//! tag `musubi-basic-v1`, so one variant never opens as the other.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use musubi_model::capability::ProviderId;
use musubi_model::control::{AccountTokens, Credentials, Scope, SealedTokens};
use musubi_model::{AccountId, ControlStore, StoreError, TenantId, TokenVault};
use musubi_store_sqlite::SqliteControl;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const TOKEN_PREFIX: &str = "mt1.";
const SEAL_VERSION: u8 = 1;
const SEAL_AAD: &[u8] = b"musubi-tokens-v1";
const BASIC_AAD: &[u8] = b"musubi-basic-v1";
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

#[derive(Serialize, Deserialize)]
struct Basic<S> {
    username: S,
    password: S,
}

fn aad(tag: &[u8], kid: u8, tenant: TenantId, account: AccountId) -> Vec<u8> {
    [
        tag,
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

    /// Seals OAuth tokens in the phase-1 format.
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
        self.seal_json(SEAL_AAD, tenant, account, &plain)
    }

    pub(crate) fn open(
        &self,
        tenant: TenantId,
        account: AccountId,
        sealed: &SealedTokens,
    ) -> Result<AccountTokens, StoreError> {
        let p: Plain<String> = self.open_json(SEAL_AAD, tenant, account, sealed)?;
        Ok(AccountTokens {
            refresh: SecretString::from(p.refresh),
            access: SecretString::from(p.access),
            access_until: p.until,
        })
    }

    pub(crate) fn seal_basic(
        &self,
        tenant: TenantId,
        account: AccountId,
        username: &str,
        password: &SecretString,
    ) -> SealedTokens {
        let plain = Basic {
            username,
            password: password.expose_secret(),
        };
        self.seal_json(BASIC_AAD, tenant, account, &plain)
    }

    /// Username and password sealed by [`Sealer::seal_basic`].
    pub(crate) fn open_basic(
        &self,
        tenant: TenantId,
        account: AccountId,
        sealed: &SealedTokens,
    ) -> Result<(String, SecretString), StoreError> {
        let b: Basic<String> = self.open_json(BASIC_AAD, tenant, account, sealed)?;
        Ok((b.username, SecretString::from(b.password)))
    }

    fn seal_json<T: Serialize>(
        &self,
        tag: &[u8],
        tenant: TenantId,
        account: AccountId,
        plain: &T,
    ) -> SealedTokens {
        let mut buf = Zeroizing::new(serde_json::to_vec(plain).expect("secrets serialise"));
        let nonce = random::<NONCE_LEN>();
        let mut out = Vec::with_capacity(HEAD + buf.len() + AES_256_GCM.tag_len());
        out.extend_from_slice(&[SEAL_VERSION, self.kid]);
        out.extend_from_slice(&nonce);
        let tag = self
            .key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad(tag, self.kid, tenant, account)),
                &mut buf[..],
            )
            .expect("AES-GCM input below its size limit");
        out.extend_from_slice(&buf);
        out.extend_from_slice(tag.as_ref());
        SealedTokens(out)
    }

    fn open_json<T: DeserializeOwned>(
        &self,
        tag: &[u8],
        tenant: TenantId,
        account: AccountId,
        sealed: &SealedTokens,
    ) -> Result<T, StoreError> {
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
                Aad::from(aad(tag, self.kid, tenant, account)),
                &mut buf[..],
            )
            .map_err(|_| bad())?;
        serde_json::from_slice(plain).map_err(|_| bad())
    }
}

/// Which [`Credentials`] variant an account holds, by its provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    OAuth,
    Basic,
}

fn kind(p: &ProviderId) -> Result<Kind, StoreError> {
    match p.0.as_str() {
        "google" => Ok(Kind::OAuth),
        "dav" | "icloud" => Ok(Kind::Basic),
        other => Err(StoreError::Backend(
            format!("no credentials kind for provider {other}").into(),
        )),
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
        let Some(sealed) = self.control.read_tokens(account)? else {
            return Ok(None);
        };
        Ok(Some(match kind(&a.provider)? {
            Kind::OAuth => Credentials::OAuth(self.sealer.open(a.tenant, account, &sealed)?),
            Kind::Basic => {
                let (username, password) = self.sealer.open_basic(a.tenant, account, &sealed)?;
                Credentials::Basic { username, password }
            }
        }))
    }

    fn store(&self, account: AccountId, c: &Credentials) -> Result<(), StoreError> {
        let a = self.control.account(account)?.ok_or(StoreError::NotFound)?;
        let sealed = match (kind(&a.provider)?, c) {
            (Kind::OAuth, Credentials::OAuth(t)) => self.sealer.seal(a.tenant, account, t),
            (Kind::Basic, Credentials::Basic { username, password }) => self
                .sealer
                .seal_basic(a.tenant, account, username, password),
            (k, _) => {
                return Err(StoreError::Backend(
                    format!("provider {} takes {k:?} credentials", a.provider.0).into(),
                ));
            }
        };
        self.control.write_tokens(account, &sealed)
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
    fn basic_credentials_open_only_as_basic_for_their_account() {
        let s = Sealer::new(&[7; 32]);
        let (t, a) = (TenantId::from_u128(1), AccountId::from_u128(2));
        let pw = SecretString::from("app-specific");
        let sealed = s.seal_basic(t, a, "me@example.com", &pw);
        let (user, back) = s.open_basic(t, a, &sealed).unwrap();
        assert_eq!(user, "me@example.com");
        assert_eq!(back.expose_secret(), "app-specific");
        assert!(s.open_basic(t, AccountId::from_u128(3), &sealed).is_err());
        assert!(s.open_basic(TenantId::from_u128(9), a, &sealed).is_err());
        // The variants use different additional data.
        assert!(s.open(t, a, &sealed).is_err(), "Basic opened as OAuth");
        let oauth = s.seal(t, a, &tokens());
        assert!(s.open_basic(t, a, &oauth).is_err(), "OAuth opened as Basic");
    }

    fn vault() -> (tempfile::TempDir, Vault) {
        let d = tempfile::tempdir().unwrap();
        let control = SqliteControl::open(&d.path().join("control.db")).unwrap();
        control.insert_tenant(TenantId::from_u128(1)).unwrap();
        let v = Vault {
            control: Arc::new(control),
            sealer: Arc::new(Sealer::new(&[7; 32])),
        };
        (d, v)
    }

    fn add_account(v: &Vault, id: u128, provider: &str) -> AccountId {
        let id = AccountId::from_u128(id);
        v.control
            .upsert_account(&musubi_model::control::AccountRecord {
                id,
                tenant: TenantId::from_u128(1),
                provider: ProviderId(provider.into()),
                subject: format!("s{id}"),
                state: musubi_model::control::AccountState::Active,
            })
            .unwrap();
        id
    }

    #[test]
    fn vault_picks_the_variant_by_provider() {
        let (_d, v) = vault();
        let google = add_account(&v, 1, "google");
        let dav = add_account(&v, 2, "dav");
        let icloud = add_account(&v, 3, "icloud");
        let basic = || Credentials::Basic {
            username: "u".into(),
            password: SecretString::from("p"),
        };
        assert!(v.load(google).unwrap().is_none());
        assert!(v.load(AccountId::from_u128(99)).unwrap().is_none());

        v.store(google, &Credentials::OAuth(tokens())).unwrap();
        assert!(matches!(
            v.load(google).unwrap(),
            Some(Credentials::OAuth(_))
        ));
        for id in [dav, icloud] {
            v.store(id, &basic()).unwrap();
            let Some(Credentials::Basic { username, password }) = v.load(id).unwrap() else {
                panic!("Basic expected");
            };
            assert_eq!((username.as_str(), password.expose_secret()), ("u", "p"));
        }

        // A mismatched variant is refused and the stored blob stays.
        let before = v.control.read_tokens(google).unwrap();
        assert!(v.store(google, &basic()).is_err());
        assert_eq!(v.control.read_tokens(google).unwrap(), before);
        let before = v.control.read_tokens(dav).unwrap();
        assert!(v.store(dav, &Credentials::OAuth(tokens())).is_err());
        assert_eq!(v.control.read_tokens(dav).unwrap(), before);
        let none = add_account(&v, 4, "icloud");
        assert!(v.store(none, &Credentials::OAuth(tokens())).is_err());
        assert!(v.control.read_tokens(none).unwrap().is_none());

        // A blob moved to another account's row does not open there.
        v.control.write_tokens(icloud, &before.unwrap()).unwrap();
        assert!(matches!(v.load(icloud), Err(StoreError::Corrupt(_))));
        // Nor does an OAuth blob in a Basic account's row.
        let oauth = v.control.read_tokens(google).unwrap().unwrap();
        v.control.write_tokens(dav, &oauth).unwrap();
        assert!(matches!(v.load(dav), Err(StoreError::Corrupt(_))));
    }

    /// Phase-1 code and schema, copied verbatim from commit b2e7758
    /// (`schema.rs` `CONTROL`, `crypto.rs` `Plain`, `aad`, `seal`, `open`).
    mod phase1 {
        use super::*;

        pub const CONTROL: &str = r#"
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL);
INSERT OR IGNORE INTO meta VALUES ('lease', 0);
CREATE TABLE IF NOT EXISTS tenants(id BLOB PRIMARY KEY);
CREATE TABLE IF NOT EXISTS api_keys(
  hash BLOB PRIMARY KEY, tenant BLOB NOT NULL, scope TEXT NOT NULL,
  created INTEGER NOT NULL, revoked INTEGER);
CREATE INDEX IF NOT EXISTS api_keys_tenant ON api_keys(tenant);
CREATE TABLE IF NOT EXISTS accounts(
  id BLOB PRIMARY KEY, tenant BLOB NOT NULL, provider TEXT NOT NULL, subject TEXT NOT NULL,
  state TEXT NOT NULL, tokens BLOB,
  UNIQUE(tenant, provider, subject));
CREATE TABLE IF NOT EXISTS oauth_states(
  hash BLOB PRIMARY KEY, tenant BLOB NOT NULL, provider TEXT NOT NULL, account BLOB,
  verifier TEXT NOT NULL, expires INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS oauth_states_expires ON oauth_states(expires);
CREATE TABLE IF NOT EXISTS jobs(
  id INTEGER PRIMARY KEY AUTOINCREMENT, tenant BLOB NOT NULL, account BLOB,
  lane TEXT NOT NULL, kind TEXT NOT NULL, priority INTEGER NOT NULL, due INTEGER NOT NULL,
  dedup TEXT, lease INTEGER, lease_until INTEGER, attempts INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS jobs_due ON jobs(lane, priority DESC, due);
CREATE INDEX IF NOT EXISTS jobs_tenant ON jobs(tenant);
CREATE INDEX IF NOT EXISTS jobs_account ON jobs(account) WHERE account IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS jobs_dedup ON jobs(tenant, dedup)
  WHERE dedup IS NOT NULL AND lease IS NULL;
CREATE TABLE IF NOT EXISTS channels(
  id TEXT PRIMARY KEY, tenant BLOB NOT NULL, account BLOB NOT NULL, collection BLOB NOT NULL,
  remote TEXT, resource TEXT, token TEXT, expiration INTEGER);
CREATE INDEX IF NOT EXISTS channels_tenant ON channels(tenant, account);
CREATE INDEX IF NOT EXISTS channels_expiration ON channels(expiration);
"#;

        const SEAL_VERSION: u8 = 1;
        const SEAL_AAD: &[u8] = b"musubi-tokens-v1";
        const HEAD: usize = 2 + NONCE_LEN;

        pub struct Sealer {
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
            pub fn new(key: &[u8; 32]) -> Self {
                let kid = sha256(&[b"musubi-token-key".as_slice(), key].concat())[0];
                let key = UnboundKey::new(&AES_256_GCM, key).expect("32-byte AES-256 key");
                Self {
                    key: LessSafeKey::new(key),
                    kid,
                }
            }

            pub fn seal(
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

            pub fn open(
                &self,
                tenant: TenantId,
                account: AccountId,
                sealed: &SealedTokens,
            ) -> Result<AccountTokens, StoreError> {
                let bad =
                    || StoreError::Corrupt("sealed tokens do not open for this account".into());
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
    }

    /// docs/plan/phase2.md 4.3: a phase-1 `control.db` with a Google account
    /// migrates to version 2; afterwards phase-1 queries still read the account
    /// and phase-1 code still opens its blob, the old one and a newly stored one.
    #[test]
    fn phase1_reader_still_reads_a_migrated_control_db() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("control.db");
        let (t, a) = (TenantId::from_u128(1), AccountId::from_u128(2));
        let old = phase1::Sealer::new(&[7; 32]);
        let sealed = old.seal(t, a, &tokens());
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.pragma_update(None, "journal_mode", "WAL").unwrap();
            c.execute_batch(phase1::CONTROL).unwrap();
            c.execute("INSERT INTO tenants(id) VALUES (?1)", [t.to_bytes()])
                .unwrap();
            c.execute(
                "INSERT INTO accounts(id, tenant, provider, subject, state) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![a.to_bytes(), t.to_bytes(), "google", "me", "active"],
            )
            .unwrap();
            c.execute(
                "UPDATE accounts SET tokens = ?2 WHERE id = ?1",
                rusqlite::params![a.to_bytes(), sealed.0],
            )
            .unwrap();
        }
        let v = Vault {
            control: Arc::new(SqliteControl::open(&path).unwrap()),
            sealer: Arc::new(Sealer::new(&[7; 32])),
        };
        assert_eq!(
            v.control.read_tokens(a).unwrap(),
            Some(sealed),
            "blob untouched"
        );
        let Some(Credentials::OAuth(back)) = v.load(a).unwrap() else {
            panic!("phase-1 blob opens as OAuth");
        };
        assert_eq!(back.refresh.expose_secret(), "r");
        // Phase 2 writes the new columns and fresh tokens.
        v.control
            .set_endpoint(
                a,
                &musubi_model::provider::Endpoint::new("x", serde_json::Value::Null),
            )
            .unwrap();
        v.control
            .set_missing(a, &[musubi_model::EntityKind::Task])
            .unwrap();
        let mut fresh = tokens();
        fresh.access = "a2".to_owned().into();
        v.store(a, &Credentials::OAuth(fresh)).unwrap();
        drop(v);

        // The phase-1 reader.
        let c = rusqlite::Connection::open(&path).unwrap();
        let schema: u32 = c
            .query_row("SELECT v FROM meta WHERE k = 'schema'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(schema, 2);
        let row: ([u8; 16], [u8; 16], String, String, String) = c
            .query_row(
                "SELECT id, tenant, provider, subject, state FROM accounts WHERE id = ?1",
                [a.to_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                a.to_bytes(),
                t.to_bytes(),
                "google".into(),
                "me".into(),
                "active".into()
            )
        );
        let blob: Vec<u8> = c
            .query_row(
                "SELECT tokens FROM accounts WHERE id = ?1",
                [a.to_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let read = old.open(t, a, &SealedTokens(blob)).unwrap();
        assert_eq!(read.access.expose_secret(), "a2");
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
