//! What a provider sends: raw content, validators, remote keys and cursors.
//!
//! All of these are opaque to the core. Only the adapter that produced them
//! interprets them; the engine compares them for equality and stores them.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::kind::EntityKind;

/// Media type of raw content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MediaType {
    /// `application/json` (Google Calendar, Tasks and People, Microsoft Graph).
    Json,
    /// `text/calendar`: one iCalendar object (a CalDAV resource), stored as
    /// `calendar`.
    Calendar,
    /// `text/vcard`: one vCard (a CardDAV resource), stored as `vcard`.
    #[serde(rename = "vcard")]
    VCard,
}

/// Provider content exactly as received: the raw layer (proposal 2.2).
///
/// Never normalised, re-serialised or trimmed. Fields the canonical form does not
/// know survive here, and phase 3 writes are patches over the last raw state.
#[derive(Clone, PartialEq, Eq)]
pub struct Raw {
    /// How to read `body`.
    pub media_type: MediaType,
    /// The bytes as received from the provider (for JSON: one resource object).
    pub body: Vec<u8>,
}

impl Raw {
    /// Content address of the body. Computed by the engine before the write
    /// transaction (proposal 5.6), stored by the store as the key of the `raw` table.
    pub fn hash(&self) -> RawHash {
        RawHash(Sha256::digest(&self.body).into())
    }
}

impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Bodies can be large and contain personal data; log their size only.
        f.debug_struct("Raw")
            .field("media_type", &self.media_type)
            .field("len", &self.body.len())
            .finish()
    }
}

/// SHA-256 of [`Raw::body`]. Equal hash means equal content: no decode and no write.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawHash(pub [u8; 32]);

impl fmt::Debug for RawHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawHash(")?;
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

/// The provider's version tag of one remote object (Google `etag`, DAV `ETag`).
///
/// Compared for equality only. Same validator as stored means the object did not
/// change: no decode, no write, no journal entry. Phase 3 sends it back in
/// conditional writes, so it is stored exactly as received: for DAV the
/// `getetag` value unchanged, including quotes and a `W/` prefix.
///
/// A DAV server without (or with an empty) `getetag` gets the synthetic
/// validator `musubi:sha256:<64 lowercase hex digits>` of the body. A validator
/// with the `musubi:` prefix is never sent in `If-Match`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Validator(pub String);

/// Identity of a remote object inside its collection (Google event `id`, DAV href).
///
/// Unique per collection; the store keeps one item per (collection, key).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteKey(pub String);

/// Identity of a remote collection inside its account (Google `calendarId`, DAV href).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CollectionRef(pub String);

/// A remote object addressed across collections of one account.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoteRef {
    /// The collection.
    pub collection: CollectionRef,
    /// The object inside it.
    pub key: RemoteKey,
}

/// One remote object as fetched: key, kind, validator and unchanged content.
///
/// A DAV resource with several components (a series master and its
/// exceptions) is one object with `master: None`; its body stays whole.
/// Non-exhaustive: build it with [`RemoteObject::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteObject {
    /// Identity in the collection.
    pub key: RemoteKey,
    /// Kind of this object, which may differ from the collection's main kind
    /// (a CalDAV calendar may mix events and tasks, an address book contacts
    /// and groups). The item is stored with it and decoded by its codec.
    pub kind: EntityKind,
    /// Version tag received together with `raw`.
    pub validator: Validator,
    /// The content, unchanged.
    pub raw: Raw,
    /// Set for a single instance of a series: key of the series master in the
    /// same collection (Google `recurringEventId`). Exceptions join their master
    /// by it, never by iCalendar UID (plan ch. 9, question 3).
    pub master: Option<RemoteKey>,
}

impl RemoteObject {
    /// An object that is not a series exception (`master: None`).
    pub fn new(key: RemoteKey, kind: EntityKind, validator: Validator, raw: Raw) -> Self {
        RemoteObject {
            key,
            kind,
            validator,
            raw,
            master: None,
        }
    }
}

/// Opaque resume position of an adapter in one collection (proposal 2.3).
///
/// A sync token, a page token, a delta link: only the adapter that created it
/// reads it. The engine stores it together with the page it came with (one
/// transaction) and hands it back unchanged.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Cursor(pub Vec<u8>);

impl fmt::Debug for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cursor({} bytes)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_hash_is_sha256_of_body() {
        let raw = Raw {
            media_type: MediaType::Json,
            body: b"abc".to_vec(),
        };
        let hex: String = raw.hash().0.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn media_type_data_names() {
        let names = [MediaType::Json, MediaType::Calendar, MediaType::VCard]
            .map(|m| serde_json::to_value(m).unwrap());
        assert_eq!(names, ["json", "calendar", "vcard"]);
    }
}
