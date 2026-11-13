//! vCard 3/4 → [`ContactCard`] and [`ContactGroupCard`].
//!
//! Pure and total, like [`crate::IcalCodec`]. The kind follows the content:
//! `KIND:group` (vCard 4) or Apple's `X-ADDRESSBOOKSERVER-KIND:group` (vCard 3)
//! is a group, anything else a card; the other codec answers
//! [`Decoded::Unsupported`]. `urn:uuid:` is cut from `UID` and from members,
//! so Apple's `X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:<uid>` matches the member's
//! bare `UID`.
//!
//! Mapping notes:
//!
//! - `TYPE=work` is the context `work`, `TYPE=home` is `private`; other types
//!   are kept lowercase (phones: `voice`, `cell`, … are features); `pref` in
//!   `TYPE` (vCard 3) is `pref: 1`, a `PREF` parameter (vCard 4) its value;
//!   `internet` is dropped.
//! - Apple's `itemN.X-ABLabel` labels the property of the same group;
//!   `_$!<Mobile>!$_` is read as `Mobile`.
//! - Apple's `X-APPLE-OMIT-YEAR` and vCard 4 `--MMDD` give a date without year.
//! - An embedded photo keeps only its size and media type.

use calcard::common::PartialDateTime;
use calcard::vcard::{VCard, VCardEntry, VCardParameterName, VCardProperty, VCardValue};
use jiff::Timestamp;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use musubi_model::contact::{
    Address, Anniversary, AnniversaryKind, CardKind, EmailAddress, Link, Name, NameComponent,
    NameComponentKind, OnlineService, Organization, PartialDate, Phone, Photo,
};
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{Contact, ContactCard, ContactGroup, ContactGroupCard, Raw};

/// `Codec<Contact>` and `Codec<ContactGroup>` for CardDAV resources.
#[derive(Debug, Clone, Copy, Default)]
pub struct VCardCodec;

impl Codec<Contact> for VCardCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<ContactCard> {
        decoded(parse(raw).and_then(|c| card(&c)))
    }
}

impl Codec<ContactGroup> for VCardCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<ContactGroupCard> {
        decoded(parse(raw).and_then(|c| group(&c)))
    }
}

type Res<T> = Result<T, String>;

fn decoded<T>(r: Res<T>) -> Decoded<T> {
    match r {
        Ok(item) => Decoded::Canonical(item),
        Err(reason) => Decoded::Unsupported { reason },
    }
}

fn parse(raw: &Raw) -> Res<VCard> {
    if raw.media_type != MediaType::VCard {
        return Err(format!("unexpected media type {:?}", raw.media_type));
    }
    VCard::parse(&*String::from_utf8_lossy(&raw.body)).map_err(|e| format!("not a vCard: {e:?}"))
}

fn other(name: &str) -> VCardProperty {
    VCardProperty::Other(name.to_owned())
}

/// All entries named `prop`, also as an `Other` name (calcard keeps
/// non-standard names as written).
fn entries<'a>(c: &'a VCard, names: &[&str]) -> impl Iterator<Item = &'a VCardEntry> {
    let names: Vec<String> = names.iter().map(|n| n.to_ascii_uppercase()).collect();
    c.entries.iter().filter(move |e| {
        names
            .iter()
            .any(|n| e.name.as_str().eq_ignore_ascii_case(n))
    })
}

fn first_text<'a>(c: &'a VCard, names: &[&str]) -> Option<&'a str> {
    entries(c, names)
        .find_map(|e| e.values.first())
        .and_then(VCardValue::as_text)
}

/// The text of every value; a comma list (`Component`) gives each item.
fn texts(e: &VCardEntry) -> Vec<&str> {
    e.values
        .iter()
        .flat_map(|v| match v {
            VCardValue::Component(items) => items.iter().map(String::as_str).collect(),
            other => other.as_text().into_iter().collect::<Vec<_>>(),
        })
        .collect()
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

fn strip_urn(s: &str) -> &str {
    s.get(..9)
        .filter(|p| p.eq_ignore_ascii_case("urn:uuid:"))
        .map_or(s, |_| &s[9..])
}

fn uid(c: &VCard) -> Res<String> {
    c.uid()
        .map(strip_urn)
        .and_then(non_empty)
        .ok_or_else(|| "vCard without UID".to_owned())
}

fn kind(c: &VCard) -> Option<&str> {
    first_text(c, &["KIND", "X-ADDRESSBOOKSERVER-KIND"])
}

fn is_group(c: &VCard) -> bool {
    kind(c).is_some_and(|k| k.eq_ignore_ascii_case("group"))
}

fn group(c: &VCard) -> Res<ContactGroupCard> {
    if !is_group(c) {
        return Err("vCard is not a group".into());
    }
    let mut g = ContactGroupCard::new(uid(c)?);
    g.name = first_text(c, &["FN"]).unwrap_or_default().to_owned();
    g.members = entries(c, &["MEMBER", "X-ADDRESSBOOKSERVER-MEMBER"])
        .flat_map(texts)
        .filter_map(|m| non_empty(strip_urn(m)))
        .collect();
    Ok(g)
}

fn card(c: &VCard) -> Res<ContactCard> {
    if is_group(c) {
        return Err("vCard is a group".into());
    }
    let mut card = ContactCard::new(uid(c)?);
    card.kind = match kind(c).map(str::to_ascii_lowercase).as_deref() {
        Some("org") => CardKind::Org,
        Some("location") => CardKind::Location,
        _ => CardKind::Individual,
    };
    card.name = name(c);
    card.nicknames = entries(c, &["NICKNAME"])
        .flat_map(texts)
        .filter_map(non_empty)
        .collect();
    card.organizations = entries(c, &["ORG"])
        .filter_map(|e| {
            let parts = texts(e);
            let mut parts = parts.into_iter().map(non_empty);
            let mut org = Organization::default();
            org.name = parts.next().flatten();
            org.units = parts.flatten().collect();
            (org.name.is_some() || !org.units.is_empty()).then_some(org)
        })
        .collect();
    card.titles = entries(c, &["TITLE"])
        .flat_map(texts)
        .filter_map(non_empty)
        .collect();
    card.emails = entries(c, &["EMAIL"])
        .filter_map(|e| {
            let mut m = EmailAddress::new(non_empty(texts(e).first()?)?);
            let t = types(e);
            m.contexts = t.contexts;
            m.pref = pref(e, t.pref);
            m.label = label(c, e);
            Some(m)
        })
        .collect();
    card.phones = entries(c, &["TEL"])
        .filter_map(|e| {
            let mut p = Phone::new(non_empty(texts(e).first()?)?);
            let t = types(e);
            p.features = t.features;
            p.contexts = t.contexts;
            p.pref = pref(e, t.pref);
            p.label = label(c, e);
            Some(p)
        })
        .collect();
    card.addresses = entries(c, &["ADR"]).filter_map(address).collect();
    card.online_services = entries(c, &["IMPP", "SOCIALPROFILE", "X-SOCIALPROFILE"])
        .filter_map(|e| {
            let mut s = OnlineService::new(non_empty(texts(e).first()?)?);
            s.service = param(e, "X-SERVICE-TYPE")
                .or_else(|| param(e, "SERVICE-TYPE"))
                .or_else(|| {
                    type_names(e)
                        .into_iter()
                        .find(|t| !matches!(t.as_str(), "pref" | "work" | "home"))
                })
                .and_then(|s| non_empty(&s));
            s.pref = pref(e, types(e).pref);
            Some(s)
        })
        .collect();
    card.links = entries(c, &["URL"])
        .filter_map(|e| {
            let mut l = Link::new(non_empty(texts(e).first()?)?);
            l.pref = pref(e, types(e).pref);
            l.label = label(c, e);
            Some(l)
        })
        .collect();
    card.notes = entries(c, &["NOTE"])
        .flat_map(texts)
        .filter_map(non_empty)
        .collect();
    for (prop, kind) in [
        ("BDAY", AnniversaryKind::Birth),
        ("ANNIVERSARY", AnniversaryKind::Wedding),
        ("DEATHDATE", AnniversaryKind::Death),
    ] {
        card.anniversaries.extend(
            entries(c, &[prop]).filter_map(|e| partial_date(e).map(|d| Anniversary::new(kind, d))),
        );
    }
    card.photos = entries(c, &["PHOTO"]).filter_map(photo).collect();
    card.updated = entries(c, &["REV"])
        .find_map(|e| e.values.first())
        .and_then(VCardValue::as_partial_date_time)
        .and_then(timestamp);
    Ok(card)
}

/// `N` components (RFC 6350 order, plus RFC 9554 surname2 and generation) and
/// `FN`.
fn name(c: &VCard) -> Option<Name> {
    const KINDS: [NameComponentKind; 7] = [
        NameComponentKind::Surname,
        NameComponentKind::Given,
        NameComponentKind::Given2,
        NameComponentKind::Title,
        NameComponentKind::Credential,
        NameComponentKind::Surname2,
        NameComponentKind::Generation,
    ];
    let mut name = Name::default();
    if let Some(e) = entries(c, &["N"]).next() {
        for (kind, v) in KINDS.iter().zip(&e.values) {
            let items = match v {
                VCardValue::Component(items) => items.iter().map(String::as_str).collect(),
                other => other.as_text().into_iter().collect::<Vec<_>>(),
            };
            for item in items.into_iter().filter_map(non_empty) {
                name.components.push(NameComponent::new(*kind, item));
            }
        }
    }
    name.full = first_text(c, &["FN"]).and_then(non_empty);
    (name.full.is_some() || !name.components.is_empty()).then_some(name)
}

/// `TYPE` values, lowercase, comma lists split.
fn type_names(e: &VCardEntry) -> Vec<String> {
    e.params
        .iter()
        .filter(|p| p.name == VCardParameterName::Type)
        .filter_map(|p| p.value.as_text())
        .flat_map(|t| t.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

#[derive(Default)]
struct Types {
    contexts: Vec<String>,
    features: Vec<String>,
    pref: bool,
}

/// Phone features of RFC 6350 `TYPE` (plus vCard 3 `msg`, `bbs`, `modem`,
/// `car`, `isdn`, `pcs`).
const FEATURES: &[&str] = &[
    "voice",
    "fax",
    "cell",
    "video",
    "pager",
    "text",
    "textphone",
    "main-number",
    "msg",
    "bbs",
    "modem",
    "car",
    "isdn",
    "pcs",
    "iphone",
];

fn types(e: &VCardEntry) -> Types {
    let phone = e.name == VCardProperty::Tel;
    let mut t = Types::default();
    for name in type_names(e) {
        match name.as_str() {
            "pref" => t.pref = true,
            "internet" | "x400" => {}
            "work" => t.contexts.push(name),
            "home" => t.contexts.push("private".into()),
            f if phone && FEATURES.contains(&f) => t.features.push(name),
            _ => t.contexts.push(name),
        }
    }
    t
}

/// `PREF=n` (vCard 4, 1–100), else 1 for vCard 3 `TYPE=pref`.
fn pref(e: &VCardEntry, type_pref: bool) -> Option<u8> {
    e.params
        .iter()
        .find(|p| p.name == VCardParameterName::Pref)
        .and_then(|p| match p.value.as_integer() {
            Some(calcard::common::IanaType::Iana(n)) => Some(n),
            Some(calcard::common::IanaType::Other(s)) => s.trim().parse().ok(),
            None => None,
        })
        .map(|n| u8::try_from(n.clamp(1, 100)).unwrap_or(100))
        .or(type_pref.then_some(1))
}

fn param(e: &VCardEntry, name: &str) -> Option<String> {
    e.params
        .iter()
        .find(|p| p.name.as_str().eq_ignore_ascii_case(name))
        .and_then(|p| p.value.as_text())
        .map(str::to_owned)
}

/// Apple's `X-ABLabel` of the entry's group.
fn label(c: &VCard, e: &VCardEntry) -> Option<String> {
    let group = e.group.as_deref()?;
    let text = c
        .entries
        .iter()
        .filter(|l| {
            l.group
                .as_deref()
                .is_some_and(|g| g.eq_ignore_ascii_case(group))
        })
        .find(|l| {
            l.name == other("X-ABLabel") || l.name.as_str().eq_ignore_ascii_case("X-ABLABEL")
        })?
        .values
        .first()?
        .as_text()?;
    let text = text
        .strip_prefix("_$!<")
        .and_then(|t| t.strip_suffix(">!$_"))
        .unwrap_or(text);
    non_empty(text)
}

/// `ADR`: PO box; extended; street; locality; region; postcode; country.
fn address(e: &VCardEntry) -> Option<Address> {
    let parts: Vec<Option<String>> = e
        .values
        .iter()
        .map(|v| match v {
            VCardValue::Component(items) => non_empty(&items.join(", ")),
            other => other.as_text().and_then(non_empty),
        })
        .collect();
    let part = |i: usize| parts.get(i).cloned().flatten();
    let street: Vec<String> = [2, 1, 0].into_iter().filter_map(part).collect();
    let t = types(e);
    let mut a = Address::default();
    a.full = param(e, "LABEL").and_then(|l| non_empty(&l));
    a.street = (!street.is_empty()).then(|| street.join(", "));
    a.locality = part(3);
    a.region = part(4);
    a.postcode = part(5);
    a.country = part(6);
    a.contexts = t.contexts;
    a.pref = pref(e, t.pref);
    let empty = a.full.is_none()
        && a.street.is_none()
        && a.locality.is_none()
        && a.region.is_none()
        && a.postcode.is_none()
        && a.country.is_none();
    (!empty).then_some(a)
}

fn partial_date(e: &VCardEntry) -> Option<PartialDate> {
    let d = e.values.first()?.as_partial_date_time()?;
    let omit = param(e, "X-APPLE-OMIT-YEAR").and_then(|y| y.trim().parse::<u16>().ok());
    let year = d.year.filter(|y| Some(*y) != omit);
    let mut out = PartialDate::default();
    out.year = year.and_then(|y| i16::try_from(y).ok());
    out.month = d.month.filter(|m| (1..=12).contains(m));
    out.day = d.day.filter(|d| (1..=31).contains(d));
    (out.month.is_some() || out.year.is_some()).then_some(out)
}

fn photo(e: &VCardEntry) -> Option<Photo> {
    let pref = pref(e, types(e).pref);
    match e.values.first()? {
        VCardValue::Binary(data) => Some(Photo::Embedded {
            size: u64::try_from(data.data.len()).unwrap_or(u64::MAX),
            media_type: data.content_type.clone(),
            pref,
        }),
        other => Some(Photo::Uri {
            uri: non_empty(other.as_text()?)?,
            pref,
        }),
    }
}

/// `REV`; floating times and dates are read as UTC.
fn timestamp(d: &PartialDateTime) -> Option<Timestamp> {
    let date = Date::new(
        i16::try_from(d.year?).ok()?,
        i8::try_from(d.month?).ok()?,
        i8::try_from(d.day?).ok()?,
    )
    .ok()?;
    let small = |v: Option<u8>| i8::try_from(v.unwrap_or(0)).ok();
    let time = Time::new(
        small(d.hour)?,
        small(d.minute)?,
        small(d.second)?.min(59),
        0,
    )
    .ok()?;
    let secs = i64::from(d.tz_hour.unwrap_or(0)) * 3600 + i64::from(d.tz_minute.unwrap_or(0)) * 60;
    let offset = if d.tz_minus { -secs } else { secs };
    let local = date
        .to_datetime(time)
        .to_zoned(TimeZone::UTC)
        .ok()?
        .timestamp();
    local
        .checked_sub(jiff::SignedDuration::from_secs(offset))
        .ok()
}
