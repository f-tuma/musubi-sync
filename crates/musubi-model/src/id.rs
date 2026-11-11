//! Identifiers.
//!
//! Four identities must never be merged (proposal 2.4): [`EntityId`] (our record),
//! [`ItemId`] (our id of one copy in one account), the remote key
//! ([`crate::remote::RemoteKey`] inside a collection, plus the validator) and the
//! correlation key (iCalendar UID + RECURRENCE-ID, kept in the canonical document).
//!
//! All our identifiers are 128 bits. Their text form is RFC 4648 base32hex,
//! lowercase, without padding: exactly 26 characters from `0-9a-v`. That text:
//!
//! - sorts in the same order as the numbers,
//! - is safe in file names ([`TenantId`] is a directory name under `<data>/tenants/`),
//! - is a valid Google client-generated event id (base32hex, 5–1024 characters),
//!   so phase 3 can derive a provider id from a [`PlacementId`] without a mapping.
//!
//! Who creates identifiers: the store assigns entity, item, placement and
//! collection ids when it inserts a row; the control plane assigns tenant and
//! account ids. This crate has no randomness: the creator draws 128 random bits
//! and calls `from_u128`. Parsing ([`std::str::FromStr`]) validates the text
//! strictly, because a [`TenantId`] goes into a path and identifiers arrive from
//! API callers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const ALPHABET: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";
/// Length of the text form of a 128-bit identifier.
pub const ID_TEXT_LEN: usize = 26;

fn encode(v: u128) -> [u8; ID_TEXT_LEN] {
    let mut out = [0u8; ID_TEXT_LEN];
    for (i, c) in out.iter_mut().enumerate() {
        // 26 groups of 5 bits cover 130 bits; the last group is padded with two zero bits.
        let shift = 128 - 5 * (i as i32 + 1);
        let group = if shift >= 0 { v >> shift } else { v << -shift };
        *c = ALPHABET[(group & 31) as usize];
    }
    out
}

fn decode(s: &str) -> Result<u128, ParseIdError> {
    let bytes = s.as_bytes();
    if bytes.len() != ID_TEXT_LEN {
        return Err(ParseIdError::Length(bytes.len()));
    }
    let mut v: u128 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'v' => b - b'a' + 10,
            _ => return Err(ParseIdError::Character(i)),
        } as u128;
        if i == ID_TEXT_LEN - 1 {
            if d & 0b11 != 0 {
                // Non-canonical: the padding bits must be zero, so every id has one text form.
                return Err(ParseIdError::Character(i));
            }
            v = (v << 3) | (d >> 2);
        } else {
            v = (v << 5) | d;
        }
    }
    Ok(v)
}

/// Why a string is not a valid identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseIdError {
    /// The text is not exactly 26 characters long (the value is the actual length).
    #[error("identifier must be {ID_TEXT_LEN} characters, got {0}")]
    Length(usize),
    /// The character at this byte offset is outside lowercase base32hex, or the
    /// last character has non-zero padding bits.
    #[error("invalid identifier character at offset {0}")]
    Character(usize),
}

macro_rules! id128 {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u128);

        impl $name {
            /// Wraps 128 bits. The caller is responsible for uniqueness (random bits).
            pub const fn from_u128(v: u128) -> Self {
                Self(v)
            }

            /// The 128 bits, for storage as a 16-byte blob or two integers.
            pub const fn as_u128(self) -> u128 {
                self.0
            }

            /// Big-endian bytes, the form stored in a database blob.
            pub const fn to_bytes(self) -> [u8; 16] {
                self.0.to_be_bytes()
            }

            /// Inverse of [`Self::to_bytes`].
            pub const fn from_bytes(b: [u8; 16]) -> Self {
                Self(u128::from_be_bytes(b))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let text = encode(self.0);
                // The alphabet is ASCII, so this cannot fail.
                f.write_str(std::str::from_utf8(&text).map_err(|_| fmt::Error)?)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;
            fn from_str(s: &str) -> Result<Self, ParseIdError> {
                decode(s).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

id128! {
    /// A tenant: one isolated customer of the instance, with its own `main.db`.
    /// Random. Its text form is the directory name `<data>/tenants/<tenant_id>/`;
    /// nothing else from a caller ever goes into a path.
    TenantId
}
id128! {
    /// A connected provider account (one OAuth grant). Assigned by the control plane.
    AccountId
}
id128! {
    /// Our id of a remote collection (a calendar). Assigned by the tenant store in
    /// [`crate::store::TenantStore::apply_discovery`].
    CollectionId
}
id128! {
    /// Our stable id of a record that applications work with (proposal 2.4).
    /// Assigned by the tenant store when it creates the entity.
    EntityId
}
id128! {
    /// Our id of one copy of a record in one account. Assigned by the tenant store.
    ItemId
}
id128! {
    /// Our id of the binding between an entity and an item. Assigned by the tenant
    /// store. Phase 3 derives the provider's client id for a new remote object from it.
    PlacementId
}

/// Position in a tenant's change journal.
///
/// Assigned by the tenant store on commit. Strictly increasing and never reused,
/// not even after [`crate::store::TenantStore::prune`] or a crash
/// (`AUTOINCREMENT`). `Seq(0)` means "before the first change".
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Seq(pub u64);

/// Identifies one full read of one collection.
///
/// Assigned by the engine when a full read starts. It must be greater than every
/// `ReadId` used for that collection before (one more than
/// [`crate::store::CollectionState::last_read`] is always valid). Pages written with it mark the items they saw;
/// [`crate::store::TenantStore::finish_full_read`] then tombstones items it did not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReadId(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip_and_order() {
        let samples = [0u128, 1, 31, 32, u64::MAX as u128, u128::MAX - 1, u128::MAX];
        for v in samples {
            let id = TenantId::from_u128(v);
            let text = id.to_string();
            assert_eq!(text.len(), ID_TEXT_LEN);
            assert_eq!(text.parse::<TenantId>(), Ok(id));
        }
        let mut texts: Vec<String> = samples
            .iter()
            .map(|&v| EntityId::from_u128(v).to_string())
            .collect();
        let sorted = texts.clone();
        texts.sort();
        assert_eq!(texts, sorted, "text order must equal numeric order");
        assert_eq!(
            TenantId::from_u128(0).to_string(),
            "00000000000000000000000000"
        );
        assert_eq!(
            TenantId::from_u128(u128::MAX).to_string(),
            "vvvvvvvvvvvvvvvvvvvvvvvvvs"
        );
    }

    #[test]
    fn rejects_non_canonical_text() {
        assert_eq!("".parse::<TenantId>(), Err(ParseIdError::Length(0)));
        assert_eq!(
            "../../../../../../etc/pass".parse::<TenantId>(),
            Err(ParseIdError::Character(0))
        );
        assert_eq!(
            "0000000000000000000000000W".parse::<TenantId>(),
            Err(ParseIdError::Character(25))
        );
        // Last character with padding bits set.
        assert_eq!(
            "00000000000000000000000001".parse::<TenantId>(),
            Err(ParseIdError::Character(25))
        );
        assert_eq!(
            "vvvvvvvvvvvvvvvvvvvvvvvvvv".parse::<TenantId>(),
            Err(ParseIdError::Character(25))
        );
    }

    #[test]
    fn serde_uses_text_form() {
        let id = ItemId::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ItemId>(&json).ok(), Some(id));
        assert!(serde_json::from_str::<ItemId>("\"short\"").is_err());
        assert_eq!(ItemId::from_bytes(id.to_bytes()), id);
    }
}
