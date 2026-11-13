//! Google People (plan 6.3): one fixed [`EntityKind::Contact`] collection,
//! `people.connections.list` with a sync token, and [`PeopleCodec`].
//!
//! A full read requests a sync token; deltas read with it and see deleted
//! contacts as `metadata.deleted: true`. The token expires (7 days after the
//! full read that issued it), so roughly once a week the delta answers
//! `EXPIRED_SYNC_TOKEN` and the engine starts a full read again.

use jiff::Timestamp;
use musubi_model::contact::{
    Address, Anniversary, AnniversaryKind, EmailAddress, Link, Name, NameComponent,
    NameComponentKind, Organization, PartialDate, Phone, Photo,
};
use musubi_model::provider::{
    Access, BoxStream, Codec, Decoded, FetchHint, Pass, RemoteCollection,
};
use musubi_model::remote::MediaType;
use musubi_model::{
    AccountCx, ChangePage, CollectionRef, Contact, ContactCard, Cursor, EntityKind, Raw, RemoteKey,
    RemoteObject, SyncError,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::provider::{
    GoogleProvider, body, error_for, excerpt, has_reason, join, pages, permanent, raw_object,
};

/// The contacts collection: there is exactly one per account.
pub(crate) const COLLECTION: &str = "people/me/connections";
/// Largest `connections.list` page Google serves.
const MAX_PAGE: u32 = 1000;
/// Everything [`PeopleCodec`] reads, plus what stays in the raw layer only
/// (`memberships`, `clientData`). Part of the cursor: Google rejects a sync
/// token issued for other fields, so a change here forces a full read.
pub(crate) const PERSON_FIELDS: &str = "addresses,biographies,birthdays,clientData,\
emailAddresses,memberships,metadata,names,nicknames,organizations,phoneNumbers,photos,urls";

/// What the engine stores as [`Cursor`] for the contacts (plan 3.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PeopleCursor {
    /// A full read: mid-pass with `page`; `None` after a last page that
    /// brought no sync token, so the next pass reads everything again.
    Full {
        page: Option<String>,
        fields: String,
    },
    /// A delta from `token`; `page` while the delta spans several pages.
    Sync {
        token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<String>,
        fields: String,
    },
}

/// One `connections.list` request of a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Query {
    /// The sync token of a delta.
    token: Option<String>,
    page: Option<String>,
}

impl Query {
    fn pass(&self) -> Pass {
        if self.token.is_some() {
            Pass::Delta
        } else {
            Pass::Full
        }
    }

    /// Contacts have no window: every start without a cursor is a full read.
    fn start(cursor: Option<Cursor>) -> Result<Self, SyncError> {
        let parsed = match cursor {
            None => {
                return Ok(Query {
                    token: None,
                    page: None,
                });
            }
            Some(c) => serde_json::from_slice(&c.0),
        };
        match parsed {
            Ok(PeopleCursor::Full { page, fields }) if fields == PERSON_FIELDS => {
                Ok(Query { token: None, page })
            }
            Ok(PeopleCursor::Sync {
                token,
                page,
                fields,
            }) if fields == PERSON_FIELDS => Ok(Query {
                token: Some(token),
                page,
            }),
            Ok(_) => {
                tracing::info!("contacts cursor is for other personFields; full read");
                Err(SyncError::CursorExpired)
            }
            Err(e) => {
                tracing::warn!(error = %e, "unreadable contacts cursor; full read");
                Err(SyncError::CursorExpired)
            }
        }
    }

    fn cursor(&self, page: Option<String>) -> PeopleCursor {
        let fields = PERSON_FIELDS.to_owned();
        match &self.token {
            Some(token) => PeopleCursor::Sync {
                token: token.clone(),
                page,
                fields,
            },
            None => PeopleCursor::Full { page, fields },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionsPage {
    #[serde(default)]
    connections: Vec<Box<RawValue>>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
}

/// The fields of a person the transport itself needs.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonMeta {
    resource_name: String,
    etag: Option<String>,
    #[serde(default)]
    metadata: DeletedMeta,
}

#[derive(Deserialize, Default)]
struct DeletedMeta {
    #[serde(default)]
    deleted: bool,
}

fn person_meta(raw: &RawValue) -> Result<PersonMeta, SyncError> {
    serde_json::from_str(raw.get()).map_err(|e| permanent(format!("person: {e}")))
}

/// Turns one `connections.list` response into a page and the next request.
fn build_page(q: &Query, body: &[u8]) -> Result<(ChangePage, Option<Query>), SyncError> {
    let resp: ConnectionsPage = serde_json::from_slice(body)
        .map_err(|e| permanent(format!("people.connections.list: {e}")))?;
    let pass = q.pass();
    let mut upserts = Vec::new();
    let mut removals = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Each key at most once per page; the last occurrence wins.
    for raw in resp.connections.iter().rev() {
        let meta = person_meta(raw)?;
        if !seen.insert(meta.resource_name.clone()) {
            continue;
        }
        match (meta.metadata.deleted, pass) {
            // A full read lists live contacts only; absence deletes.
            (true, Pass::Full) => {}
            (true, _) => removals.push(RemoteKey(meta.resource_name)),
            (false, _) => upserts.push(raw_object(
                raw,
                &meta.resource_name,
                EntityKind::Contact,
                meta.etag.as_deref(),
            )),
        }
    }
    upserts.reverse();
    removals.reverse();
    let (cursor, next) = match resp.next_page_token {
        Some(page) => (
            q.cursor(Some(page.clone())),
            Some(Query {
                page: Some(page),
                ..q.clone()
            }),
        ),
        None => {
            let cursor = match resp.next_sync_token {
                Some(token) => PeopleCursor::Sync {
                    token,
                    page: None,
                    fields: PERSON_FIELDS.to_owned(),
                },
                None => {
                    tracing::warn!(
                        ?pass,
                        "connections.list: last page without a sync token; next pass reads everything"
                    );
                    PeopleCursor::Full {
                        page: None,
                        fields: PERSON_FIELDS.to_owned(),
                    }
                }
            };
            (cursor, None)
        }
    };
    let cursor = Cursor(serde_json::to_vec(&cursor).unwrap_or_default());
    let mut page = ChangePage::new(pass, Some(cursor), next.is_none());
    page.upserts = upserts;
    page.removals = removals;
    Ok((page, next))
}

impl GoogleProvider {
    fn connections_url(&self) -> reqwest::Url {
        join(&self.config.people_base, &["people", "me", "connections"])
    }

    /// Discovery: one `connections.list` with `pageSize=1` proves the scope,
    /// then the fixed contacts collection is found. A missing scope is
    /// [`SyncError::MissingScope`].
    pub(crate) async fn people_probe(
        &self,
        cx: &AccountCx,
        found: &mut Vec<RemoteCollection>,
    ) -> Result<(), SyncError> {
        let mut url = self.connections_url();
        url.query_pairs_mut()
            .append_pair("pageSize", "1")
            .append_pair("personFields", "metadata");
        self.call(cx, self.http.get(url), Some(EntityKind::Contact))
            .await?;
        found.push(RemoteCollection::new(
            CollectionRef(COLLECTION.to_owned()),
            EntityKind::Contact,
            "Kontakty",
            Access::Owner,
        ));
        Ok(())
    }

    pub(crate) fn people_changes<'a>(
        &'a self,
        cx: &'a AccountCx,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> BoxStream<'a, Result<ChangePage, SyncError>> {
        let size = hint.page_size.map_or(MAX_PAGE, |n| n.get().min(MAX_PAGE));
        pages(Query::start(cursor), move |q| self.people_page(cx, q, size))
    }

    /// One `connections.list` request.
    async fn people_page(
        &self,
        cx: &AccountCx,
        q: Query,
        size: u32,
    ) -> Result<(ChangePage, Option<Query>), SyncError> {
        let mut url = self.connections_url();
        {
            // Every page repeats every parameter of the first request.
            let mut p = url.query_pairs_mut();
            p.append_pair("personFields", PERSON_FIELDS);
            p.append_pair("pageSize", &size.to_string());
            p.append_pair("requestSyncToken", "true");
            if let Some(token) = &q.token {
                p.append_pair("syncToken", token);
            }
            if let Some(page) = &q.page {
                p.append_pair("pageToken", page);
            }
        }
        let resp = self.send(cx, self.http.get(url)).await?;
        let status = resp.status();
        if matches!(status, StatusCode::BAD_REQUEST | StatusCode::GONE) {
            let body = resp.bytes().await.unwrap_or_default();
            // Assumed (T10): an expired token is `EXPIRED_SYNC_TOKEN`, as 400
            // or 410; a rejected page token is a plain 400.
            if has_reason(&body, &["EXPIRED_SYNC_TOKEN"])
                || (status == StatusCode::GONE && q.token.is_some())
                || q.page.is_some()
            {
                tracing::info!(body = %excerpt(&body), pass = ?q.pass(), "contacts token rejected; full read");
                return Err(SyncError::CursorExpired);
            }
            return Err(permanent(format!("HTTP {status}: {}", excerpt(&body))));
        }
        if !status.is_success() {
            return Err(error_for(resp, cx.clock.now(), Some(EntityKind::Contact)).await);
        }
        build_page(&q, &body(resp).await?)
    }

    /// `people.get`; `None` for a contact that is gone or deleted.
    pub(crate) async fn fetch_person(
        &self,
        cx: &AccountCx,
        key: &RemoteKey,
    ) -> Result<Option<RemoteObject>, SyncError> {
        let id = key
            .0
            .strip_prefix("people/")
            .ok_or_else(|| permanent(format!("not a person resource name: {}", key.0)))?;
        let mut url = join(&self.config.people_base, &["people", id]);
        url.query_pairs_mut()
            .append_pair("personFields", PERSON_FIELDS);
        let resp = self.send(cx, self.http.get(url)).await?;
        match resp.status() {
            StatusCode::NOT_FOUND | StatusCode::GONE => return Ok(None),
            s if !s.is_success() => {
                return Err(error_for(resp, cx.clock.now(), Some(EntityKind::Contact)).await);
            }
            _ => {}
        }
        let body = body(resp).await?;
        let raw: Box<RawValue> =
            serde_json::from_slice(&body).map_err(|e| permanent(format!("people.get: {e}")))?;
        let meta = person_meta(&raw)?;
        Ok((!meta.metadata.deleted).then(|| {
            raw_object(
                &raw,
                &meta.resource_name,
                EntityKind::Contact,
                meta.etag.as_deref(),
            )
        }))
    }
}

/// `Codec<Contact>` for Google People `Person` resources.
///
/// `resourceName` → `uid`; the primary name (else the first) → `name`;
/// `nicknames`; `organizations` → `organizations` (`department` as the unit)
/// and their `title` → `titles`; `emailAddresses`, `phoneNumbers` and
/// `addresses` with `type` as contexts (`home` → `private`, `work` → `work`)
/// and features (`mobile` → `cell`, `…Fax` → `fax`, `…Pager` → `pager`), a
/// custom `type` as the label, `metadata.primary` as `pref: 1`; `urls` →
/// `links`; `biographies` → `notes`; `birthdays` → birth anniversaries;
/// non-default `photos` → photo URIs; the update time of the contact source →
/// `updated`. `memberships` and `clientData` stay in the raw layer only.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeopleCodec;

impl Codec<Contact> for PeopleCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<ContactCard> {
        match decode(raw) {
            Ok(card) => Decoded::Canonical(card),
            Err(reason) => Decoded::Unsupported { reason },
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Person {
    resource_name: Option<String>,
    metadata: Option<PersonMetadata>,
    names: Vec<GName>,
    nicknames: Vec<Valued>,
    organizations: Vec<GOrganization>,
    email_addresses: Vec<Typed>,
    phone_numbers: Vec<Typed>,
    addresses: Vec<GAddress>,
    urls: Vec<Typed>,
    biographies: Vec<Valued>,
    birthdays: Vec<Birthday>,
    photos: Vec<GPhoto>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PersonMetadata {
    sources: Vec<Source>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Source {
    #[serde(rename = "type")]
    kind: Option<String>,
    update_time: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FieldMetadata {
    primary: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct GName {
    metadata: FieldMetadata,
    display_name: Option<String>,
    family_name: Option<String>,
    given_name: Option<String>,
    middle_name: Option<String>,
    honorific_prefix: Option<String>,
    honorific_suffix: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Valued {
    value: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Typed {
    metadata: FieldMetadata,
    value: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GOrganization {
    name: Option<String>,
    department: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct GAddress {
    metadata: FieldMetadata,
    #[serde(rename = "type")]
    kind: Option<String>,
    formatted_value: Option<String>,
    po_box: Option<String>,
    street_address: Option<String>,
    extended_address: Option<String>,
    city: Option<String>,
    region: Option<String>,
    postal_code: Option<String>,
    country: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Birthday {
    date: Option<GDate>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GDate {
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GPhoto {
    metadata: FieldMetadata,
    url: Option<String>,
    default: Option<bool>,
}

fn pref(m: &FieldMetadata) -> Option<u8> {
    m.primary.unwrap_or(false).then_some(1)
}

/// A non-empty string.
fn text(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

/// Google `type` → (contexts, features, label). Known types map to JSContact;
/// anything else is the user's own label.
fn classify(kind: Option<String>) -> (Vec<String>, Vec<String>, Option<String>) {
    let Some(kind) = text(kind) else {
        return (vec![], vec![], None);
    };
    let (context, rest) = if let Some(rest) = kind.strip_prefix("home") {
        (Some("private"), rest)
    } else if let Some(rest) = kind.strip_prefix("work") {
        (Some("work"), rest)
    } else {
        (None, kind.as_str())
    };
    let feature = match rest {
        "" => None,
        "Mobile" | "mobile" => Some("cell"),
        "Fax" | "fax" => Some("fax"),
        "Pager" | "pager" => Some("pager"),
        // Known types without a JSContact counterpart.
        "main" | "other" | "googleVoice" if context.is_none() => None,
        _ => return (vec![], vec![], Some(kind)),
    };
    let own = |v: Option<&str>| v.map(str::to_owned).into_iter().collect();
    (own(context), own(feature), None)
}

fn decode(raw: &Raw) -> Result<ContactCard, String> {
    if raw.media_type != MediaType::Json {
        return Err(format!("unexpected media type {:?}", raw.media_type));
    }
    let p: Person =
        serde_json::from_slice(&raw.body).map_err(|e| format!("not a Google person: {e}"))?;
    let uid = text(p.resource_name).ok_or("person without resourceName")?;
    let mut c = ContactCard::new(uid);

    let mut names = p.names;
    let at = names
        .iter()
        .position(|n| n.metadata.primary == Some(true))
        .unwrap_or(0);
    if at < names.len() {
        let n = names.swap_remove(at);
        let mut name = Name::default();
        for (kind, v) in [
            (NameComponentKind::Title, n.honorific_prefix),
            (NameComponentKind::Given, n.given_name),
            (NameComponentKind::Given2, n.middle_name),
            (NameComponentKind::Surname, n.family_name),
            (NameComponentKind::Credential, n.honorific_suffix),
        ] {
            if let Some(v) = text(v) {
                name.components.push(NameComponent::new(kind, v));
            }
        }
        name.full = text(n.display_name);
        c.name = Some(name);
    }

    c.nicknames = p
        .nicknames
        .into_iter()
        .filter_map(|n| text(n.value))
        .collect();
    for o in p.organizations {
        c.titles.extend(text(o.title));
        let (name, unit) = (text(o.name), text(o.department));
        if name.is_some() || unit.is_some() {
            let mut org = Organization::default();
            org.name = name;
            org.units = unit.into_iter().collect();
            c.organizations.push(org);
        }
    }
    for e in p.email_addresses {
        let Some(address) = text(e.value) else {
            continue;
        };
        let mut email = EmailAddress::new(address);
        (email.contexts, _, email.label) = classify(e.kind);
        email.pref = pref(&e.metadata);
        c.emails.push(email);
    }
    for t in p.phone_numbers {
        let Some(number) = text(t.value) else {
            continue;
        };
        let mut phone = Phone::new(number);
        (phone.contexts, phone.features, phone.label) = classify(t.kind);
        phone.pref = pref(&t.metadata);
        c.phones.push(phone);
    }
    for a in p.addresses {
        let mut adr = Address::default();
        let street: Vec<String> = [a.po_box, a.street_address, a.extended_address]
            .into_iter()
            .filter_map(text)
            .collect();
        adr.street = (!street.is_empty()).then(|| street.join("\n"));
        adr.full = text(a.formatted_value);
        adr.locality = text(a.city);
        adr.region = text(a.region);
        adr.postcode = text(a.postal_code);
        adr.country = text(a.country);
        (adr.contexts, _, _) = classify(a.kind);
        adr.pref = pref(&a.metadata);
        if adr != Address::default() {
            c.addresses.push(adr);
        }
    }
    for u in p.urls {
        let Some(uri) = text(u.value) else { continue };
        let mut link = Link::new(uri);
        link.label = text(u.kind);
        link.pref = pref(&u.metadata);
        c.links.push(link);
    }
    c.notes = p
        .biographies
        .into_iter()
        .filter_map(|b| text(b.value))
        .collect();
    for b in p.birthdays {
        let Some(d) = b.date else { continue };
        let mut date = PartialDate::default();
        // Google writes an unknown part as 0 or leaves it out.
        date.year = d
            .year
            .and_then(|y| i16::try_from(y).ok())
            .filter(|y| *y != 0);
        date.month = d
            .month
            .and_then(|m| u8::try_from(m).ok())
            .filter(|m| (1..=12).contains(m));
        date.day = d
            .day
            .and_then(|v| u8::try_from(v).ok())
            .filter(|v| (1..=31).contains(v));
        let a = Anniversary::new(AnniversaryKind::Birth, date);
        // The profile and the contact source may both carry the same birthday.
        if date != PartialDate::default() && !c.anniversaries.contains(&a) {
            c.anniversaries.push(a);
        }
    }
    for ph in p.photos {
        if ph.default == Some(true) {
            continue;
        }
        if let Some(uri) = text(ph.url) {
            c.photos.push(Photo::Uri {
                uri,
                pref: pref(&ph.metadata),
            });
        }
    }
    let sources = p.metadata.map(|m| m.sources).unwrap_or_default();
    let time = |s: &Source| {
        s.update_time
            .as_deref()
            .and_then(|t| t.parse::<Timestamp>().ok())
    };
    c.updated = sources
        .iter()
        .find(|s| s.kind.as_deref() == Some("CONTACT"))
        .and_then(time)
        .or_else(|| sources.iter().filter_map(time).max());
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(page: &ChangePage) -> PeopleCursor {
        serde_json::from_slice(&page.cursor.as_ref().unwrap().0).unwrap()
    }

    #[test]
    fn full_read_ends_in_a_sync_cursor_and_skips_deleted() {
        let full = Query::start(None).unwrap();
        let body = br#"{"connections":[
            {"resourceName":"people/c1","etag":"%E1"},
            {"resourceName":"people/c2","metadata":{"deleted":true}}
        ],"nextPageToken":"p2"}"#;
        let (page, next) = build_page(&full, body).unwrap();
        assert_eq!(
            (page.pass, page.last, page.upserts.len()),
            (Pass::Full, false, 1)
        );
        assert!(page.removals.is_empty());
        assert_eq!(
            Query::start(page.cursor.clone()).unwrap(),
            next.clone().unwrap()
        );
        let (page, next) =
            build_page(&next.unwrap(), br#"{"nextSyncToken":"s1","totalPeople":1}"#).unwrap();
        assert!(page.last && next.is_none());
        let delta = Query::start(page.cursor).unwrap();
        assert_eq!(
            delta,
            Query {
                token: Some("s1".into()),
                page: None
            }
        );
        let body = br#"{"connections":[{"resourceName":"people/c2","metadata":{"deleted":true}}],
            "nextSyncToken":"s2"}"#;
        let (page, _) = build_page(&delta, body).unwrap();
        assert_eq!(page.pass, Pass::Delta);
        assert_eq!(page.removals, [RemoteKey("people/c2".into())]);
        assert!(matches!(cursor(&page), PeopleCursor::Sync { token, .. } if token == "s2"));
    }

    #[test]
    fn other_person_fields_expire_the_cursor() {
        let c = PeopleCursor::Sync {
            token: "s".into(),
            page: None,
            fields: "names".into(),
        };
        let c = Cursor(serde_json::to_vec(&c).unwrap());
        assert!(matches!(
            Query::start(Some(c)),
            Err(SyncError::CursorExpired)
        ));
    }

    #[test]
    fn types_map_to_contexts_features_or_label() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            classify(Some("workMobile".into())),
            (s(&["work"]), s(&["cell"]), None)
        );
        assert_eq!(
            classify(Some("homeFax".into())),
            (s(&["private"]), s(&["fax"]), None)
        );
        assert_eq!(
            classify(Some("pager".into())),
            (s(&[]), s(&["pager"]), None)
        );
        assert_eq!(classify(Some("other".into())), (s(&[]), s(&[]), None));
        assert_eq!(
            classify(Some("Chata".into())),
            (s(&[]), s(&[]), Some("Chata".into()))
        );
        assert_eq!(
            classify(Some("homework".into())),
            (s(&[]), s(&[]), Some("homework".into()))
        );
    }
}
