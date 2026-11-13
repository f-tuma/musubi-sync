//! The canonical contact and contact group: profiles of JSContact `Card`
//! (RFC 9553).
//!
//! Sources: vCard 3/4 (CardDAV, Apple extensions) and Google People
//! (docs/plan/phase2.md, chapter 3.1). Property names are JSContact
//! (`camelCase`), with these deliberate deviations:
//!
//! - no `@type` members and no `version`;
//! - multi-valued properties are JSON arrays in source order, not maps keyed by
//!   id; the order and the `pref` of every entry are kept;
//! - `contexts` and `features` are arrays of the source's lowercase type names,
//!   open-ended (`work`, `private`, `cell`, …);
//! - photos carry only a URI or the size of embedded data, never the bytes
//!   (the bytes stay in the raw layer, chapter 3.1).
//!
//! A group (`kind: group`) is the separate type [`ContactGroupCard`], so that
//! the re-export does not clash with the [`crate::kind::ContactGroup`] marker.
//! Every change of these types that changes the JSON is a new
//! [`crate::kind::Kind::SCHEMA_VERSION`] of the respective kind.

use std::collections::BTreeSet;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// A contact card of an individual, organisation or location (JSContact `Card`).
///
/// Non-exhaustive: build it with [`ContactCard::new`] and set the other fields
/// by assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ContactCard {
    /// vCard `UID` / Google People `resourceName`.
    pub uid: String,
    /// What the card describes.
    #[serde(default)]
    pub kind: CardKind,
    /// `N` and `FN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Name>,
    /// `NICKNAME`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nicknames: Vec<String>,
    /// `ORG`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub organizations: Vec<Organization>,
    /// `TITLE` (job titles).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<String>,
    /// `EMAIL`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<EmailAddress>,
    /// `TEL`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phones: Vec<Phone>,
    /// `ADR`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<Address>,
    /// `IMPP` and social profiles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub online_services: Vec<OnlineService>,
    /// `URL`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// `NOTE`, plain text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// `BDAY`, `ANNIVERSARY`, `DEATHDATE`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anniversaries: Vec<Anniversary>,
    /// `PHOTO`: a URI or a marker of embedded data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub photos: Vec<Photo>,
    /// `REV` / Google `metadata.sources[].updateTime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<Timestamp>,
}

impl ContactCard {
    /// An individual's card with every optional field empty.
    pub fn new(uid: impl Into<String>) -> Self {
        ContactCard {
            uid: uid.into(),
            kind: CardKind::default(),
            name: None,
            nicknames: Vec::new(),
            organizations: Vec::new(),
            titles: Vec::new(),
            emails: Vec::new(),
            phones: Vec::new(),
            addresses: Vec::new(),
            online_services: Vec::new(),
            links: Vec::new(),
            notes: Vec::new(),
            anniversaries: Vec::new(),
            photos: Vec::new(),
            updated: None,
        }
    }
}

/// A contact group (JSContact `Card` with `kind: group`): vCard 4 `KIND:group`
/// or Apple `X-ADDRESSBOOKSERVER-KIND:group`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ContactGroupCard {
    /// vCard `UID`.
    pub uid: String,
    /// `FN`.
    #[serde(default)]
    pub name: String,
    /// UIDs of the members (vCard 4 `MEMBER` / Apple
    /// `X-ADDRESSBOOKSERVER-MEMBER`). The decoder always strips a `urn:uuid:`
    /// prefix; a member is compared with [`ContactCard::uid`] (also without the
    /// prefix) case-insensitively. The encoder adds the prefix back only if
    /// the source had it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub members: BTreeSet<String>,
}

impl ContactGroupCard {
    /// An unnamed group without members.
    pub fn new(uid: impl Into<String>) -> Self {
        ContactGroupCard {
            uid: uid.into(),
            name: String::new(),
            members: BTreeSet::new(),
        }
    }
}

/// JSContact `kind` of a [`ContactCard`]. Groups are [`ContactGroupCard`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CardKind {
    /// A person (default).
    #[default]
    Individual,
    /// An organisation.
    Org,
    /// A place.
    Location,
}

/// JSContact `Name`. Build it from [`Name::default`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Name {
    /// Structured components (`N`) in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<NameComponent>,
    /// The full name as displayed (`FN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<String>,
}

/// One JSContact `NameComponent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct NameComponent {
    /// What the component is.
    pub kind: NameComponentKind,
    /// Its text.
    pub value: String,
}

impl NameComponent {
    /// A component of this kind.
    pub fn new(kind: NameComponentKind, value: impl Into<String>) -> Self {
        NameComponent {
            kind,
            value: value.into(),
        }
    }
}

/// JSContact name component kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum NameComponentKind {
    /// Honorific prefix (`Dr.`).
    Title,
    /// Given name.
    Given,
    /// Additional given name.
    Given2,
    /// Family name.
    Surname,
    /// Second family name.
    Surname2,
    /// Honorific suffix (`PhD`).
    Credential,
    /// Generation (`Jr.`).
    Generation,
}

/// JSContact `Organization`. Build it from [`Organization::default`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Organization {
    /// Organisation name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Units, from the largest down.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<String>,
}

/// JSContact `EmailAddress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct EmailAddress {
    /// The address as given.
    pub address: String,
    /// Contexts (`work`, `private`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<String>,
    /// Preference 1 (most) – 100 (least); `None` = unranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pref: Option<u8>,
    /// Free-text label (Apple `X-ABLabel`, Google `formattedType`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl EmailAddress {
    /// An address without contexts, preference or label.
    pub fn new(address: impl Into<String>) -> Self {
        EmailAddress {
            address: address.into(),
            contexts: Vec::new(),
            pref: None,
            label: None,
        }
    }
}

/// JSContact `Phone`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Phone {
    /// The number (or `tel:` URI) as given.
    pub number: String,
    /// Features (`voice`, `cell`, `fax`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Contexts (`work`, `private`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<String>,
    /// Preference 1 (most) – 100 (least); `None` = unranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pref: Option<u8>,
    /// Free-text label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Phone {
    /// A number without features, contexts, preference or label.
    pub fn new(number: impl Into<String>) -> Self {
        Phone {
            number: number.into(),
            features: Vec::new(),
            contexts: Vec::new(),
            pref: None,
            label: None,
        }
    }
}

/// JSContact `Address`, flattened to the vCard `ADR` components. Build it from
/// [`Address::default`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Address {
    /// The whole address as one text (vCard 4 `LABEL` parameter, Google
    /// `formattedValue`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<String>,
    /// Street address, including the post office box and extended address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    /// City.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    /// Region or state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Postal code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    /// Country name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Contexts (`work`, `private`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<String>,
    /// Preference 1 (most) – 100 (least); `None` = unranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pref: Option<u8>,
}

/// JSContact `OnlineService`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OnlineService {
    /// URI or user name as given.
    pub uri: String,
    /// Service name (`XMPP`, `Mastodon`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Preference 1 (most) – 100 (least); `None` = unranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pref: Option<u8>,
}

impl OnlineService {
    /// A service entry without a service name or preference.
    pub fn new(uri: impl Into<String>) -> Self {
        OnlineService {
            uri: uri.into(),
            service: None,
            pref: None,
        }
    }
}

/// JSContact `Link`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Link {
    /// The URI.
    pub uri: String,
    /// Preference 1 (most) – 100 (least); `None` = unranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pref: Option<u8>,
    /// Free-text label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Link {
    /// A link without preference or label.
    pub fn new(uri: impl Into<String>) -> Self {
        Link {
            uri: uri.into(),
            pref: None,
            label: None,
        }
    }
}

/// JSContact `Anniversary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Anniversary {
    /// Which anniversary.
    pub kind: AnniversaryKind,
    /// The date, possibly without a year.
    pub date: PartialDate,
}

impl Anniversary {
    /// An anniversary of this kind on this date.
    pub fn new(kind: AnniversaryKind, date: PartialDate) -> Self {
        Anniversary { kind, date }
    }
}

/// JSContact anniversary kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AnniversaryKind {
    /// Birthday (`BDAY`).
    Birth,
    /// Date of death (`DEATHDATE`).
    Death,
    /// Wedding (`ANNIVERSARY`).
    Wedding,
}

/// JSContact `PartialDate`: any of the parts may be missing (vCard `--1124`).
/// Build it from [`PartialDate::default`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PartialDate {
    /// Year.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i16>,
    /// Month 1–12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<u8>,
    /// Day 1–31.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<u8>,
}

/// A photo of the card. Never carries image bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Photo {
    /// An external URI.
    Uri {
        /// The URI.
        uri: String,
        /// Preference 1 (most) – 100 (least); `None` = unranked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pref: Option<u8>,
    },
    /// Data embedded in the raw object (`data:` URI or base64 `PHOTO`).
    #[serde(rename_all = "camelCase")]
    Embedded {
        /// Size of the decoded data in bytes.
        size: u64,
        /// Media type, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// Preference 1 (most) – 100 (least); `None` = unranked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pref: Option<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_shape_keeps_order_and_pref() {
        let mut c = ContactCard::new("c1");
        let mut work = EmailAddress::new("b@example.com");
        work.contexts = vec!["work".into()];
        work.pref = Some(1);
        c.emails = vec![EmailAddress::new("a@example.com"), work];
        c.anniversaries = vec![Anniversary::new(
            AnniversaryKind::Birth,
            PartialDate {
                month: Some(11),
                day: Some(24),
                ..PartialDate::default()
            },
        )];
        c.photos = vec![Photo::Embedded {
            size: 2048,
            media_type: Some("image/jpeg".into()),
            pref: None,
        }];
        let value = serde_json::to_value(&c).unwrap();
        assert_eq!(
            value,
            json!({
                "uid": "c1",
                "kind": "individual",
                "emails": [
                    {"address": "a@example.com"},
                    {"address": "b@example.com", "contexts": ["work"], "pref": 1}
                ],
                "anniversaries": [{"kind": "birth", "date": {"month": 11, "day": 24}}],
                "photos": [{"source": "embedded", "size": 2048, "mediaType": "image/jpeg"}]
            })
        );
        assert_eq!(serde_json::from_value::<ContactCard>(value).unwrap(), c);

        let mut g = ContactGroupCard::new("g1");
        g.name = "Family".into();
        g.members.insert("c1".into());
        let value = serde_json::to_value(&g).unwrap();
        assert_eq!(
            value,
            json!({"uid": "g1", "name": "Family", "members": ["c1"]})
        );
        assert_eq!(
            serde_json::from_value::<ContactGroupCard>(value).unwrap(),
            g
        );
    }
}
