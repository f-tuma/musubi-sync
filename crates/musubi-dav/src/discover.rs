//! Discovery (plan, chapter 5.1): well-known, principal, home sets, collections.

use std::collections::BTreeMap;

use musubi_model::{
    AccountCx, CollectionRef, Endpoint, EntityKind,
    capability::PushSupport,
    provider::{Access, Discovery, RemoteCollection},
};
use reqwest::Url;

use crate::net::{Mode, NetError, Reply, Session, allowed, origin, scrub};
use crate::state::{Col, Level, Principals, State, normalise};
use crate::xml::{self, CALDAV, CARDDAV, CS, DAV, Multistatus, pct_decode};
use crate::{ConnectError, DavProvider};

/// Properties of a collection (step 5).
const COLLECTION_PROPS: [&str; 8] = [
    "D:resourcetype",
    "D:displayname",
    "C:supported-calendar-component-set",
    "D:supported-report-set",
    "CS:getctag",
    "D:sync-token",
    "D:current-user-privilege-set",
    "C:calendar-timezone",
];

pub(crate) async fn run(p: &DavProvider, cx: &AccountCx) -> Result<Discovery, ConnectError> {
    let endpoint = cx
        .endpoint
        .as_ref()
        .ok_or_else(|| ConnectError::InvalidConfig("no endpoint".into()))?;
    let mut st = State::from_endpoint(endpoint).map_err(ConnectError::InvalidConfig)?;
    let origins = st.origins.clone();
    let s = Session::new(&p.net, cx, &origins, Mode::Discover)?;
    let recorded = p.descents().get(&cx.account).cloned().unwrap_or_default();
    for key in &recorded {
        st.delta_override
            .entry(key.clone())
            .or_insert(Level::CtagEtag);
    }
    let mut principals = Principals::default();
    let mut homes = Vec::new();
    let mut cols = BTreeMap::new();
    let mut found = Vec::new();
    for (cal, entry) in [(true, &st.caldav_url), (false, &st.carddav_url)] {
        let Some(entry) = entry else { continue };
        let entry = Url::parse(entry).map_err(|e| ConnectError::InvalidConfig(e.to_string()))?;
        let principal = principal(&s, &entry, cal).await?;
        let normalised = Some(normalise(&principal));
        if cal {
            principals.caldav = normalised;
        } else {
            principals.carddav = normalised;
        }
        for home in home_sets(&s, &principal, cal).await? {
            for (key, col, rc) in collections(&s, &home, cal, &mut st.delta_override).await? {
                cols.insert(key, col);
                found.push(rc);
            }
            homes.push(home.to_string());
        }
    }
    if homes.is_empty() {
        return Err(ConnectError::NotDav(
            "no calendar or address book home set".into(),
        ));
    }
    st.principals = principals;
    st.homes = homes;
    st.collections = cols;
    // Persisted now: the stored endpoint carries them from here on.
    if let Some(set) = p.descents().get_mut(&cx.account) {
        set.retain(|k| !recorded.contains(k));
    }
    let mut d = Discovery::new(found);
    d.endpoint = Some(Endpoint::new(endpoint.base.clone(), st.to_json()));
    Ok(d)
}

fn conn(e: NetError) -> ConnectError {
    match e {
        NetError::Blocked(h) => ConnectError::BlockedAddress(h),
        NetError::CrossOrigin(u) => ConnectError::CrossOriginRedirect(u.to_string()),
        NetError::TooManyRedirects => ConnectError::NotDav("too many redirects".into()),
        e => ConnectError::Sync(e.into_sync()),
    }
}

fn multistatus(s: &Session<'_>, r: &Reply) -> Result<Multistatus, ConnectError> {
    match r.status {
        207 => xml::multistatus(&r.body)
            .map_err(|e| ConnectError::NotDav(format!("{}: unreadable XML: {e}", r.url))),
        401 => Err(ConnectError::Unauthorized),
        404 | 405 | 501 => Err(ConnectError::NotDav(format!(
            "{} answered {}",
            r.url, r.status
        ))),
        _ => Err(ConnectError::Sync(r.error(s.cx.clock.now()))),
    }
}

async fn propfind(
    s: &Session<'_>,
    url: &Url,
    depth: &str,
    props: &[&str],
) -> Result<(Reply, Multistatus), ConnectError> {
    let r = s
        .send("PROPFIND", url, Some(depth), xml::propfind(props))
        .await
        .map_err(conn)?;
    let ms = multistatus(s, &r)?;
    Ok((r, ms))
}

/// An href from a reply, resolved and inside the allowed origins.
fn inside(s: &Session<'_>, base: &Url, href: &str) -> Result<Url, ConnectError> {
    let url = base
        .join(href)
        .map_err(|e| ConnectError::NotDav(format!("bad href {href}: {e}")))?;
    if allowed(&url, s.origins) {
        Ok(url)
    } else {
        Err(ConnectError::CrossOriginRedirect(scrub(url).to_string()))
    }
}

/// Steps 2 and 3: well-known on an empty path, then the principal.
async fn principal(s: &Session<'_>, entry: &Url, cal: bool) -> Result<Url, ConnectError> {
    const PROPS: [&str; 2] = ["D:current-user-principal", "D:principal-URL"];
    let mut answer = None;
    if entry.path() == "/" {
        let wk = entry
            .join(if cal {
                "/.well-known/caldav"
            } else {
                "/.well-known/carddav"
            })
            .expect("constant path");
        let r = s
            .send("PROPFIND", &wk, Some("0"), xml::propfind(&PROPS))
            .await
            .map_err(conn)?;
        match r.status {
            207 => answer = Some((multistatus(s, &r)?, r.url)),
            401 => return Err(ConnectError::Unauthorized),
            // No well-known: the address itself.
            _ => {}
        }
    }
    let (ms, at) = match answer {
        Some(a) => a,
        None => {
            let (r, ms) = propfind(s, entry, "0", &PROPS).await?;
            (ms, r.url)
        }
    };
    let first = ms.responses.first();
    let href = first
        .and_then(|r| r.prop(DAV, "current-user-principal"))
        .and_then(xml::El::href)
        .or_else(|| {
            first
                .and_then(|r| r.prop(DAV, "principal-URL"))
                .and_then(xml::El::href)
        })
        .filter(|h| !h.is_empty());
    match href {
        Some(h) => inside(s, &at, h),
        None => Ok(at),
    }
}

/// Step 4.
async fn home_sets(s: &Session<'_>, principal: &Url, cal: bool) -> Result<Vec<Url>, ConnectError> {
    let (prop, ns, name) = if cal {
        ("C:calendar-home-set", CALDAV, "calendar-home-set")
    } else {
        ("CR:addressbook-home-set", CARDDAV, "addressbook-home-set")
    };
    let (r, ms) = propfind(s, principal, "0", &[prop]).await?;
    let Some(set) = ms.responses.first().and_then(|x| x.prop(ns, name)) else {
        return Ok(Vec::new());
    };
    set.all(DAV, "href")
        .map(|h| inside(s, &r.url, h.text.trim()))
        .collect()
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Step 5 and the rules below it.
async fn collections(
    s: &Session<'_>,
    home: &Url,
    cal: bool,
    overrides: &mut BTreeMap<String, Level>,
) -> Result<Vec<(String, Col, RemoteCollection)>, ConnectError> {
    let (r, ms) = propfind(s, home, "1", &COLLECTION_PROPS).await?;
    let home_key = pct_decode(home.path());
    let mut out = Vec::new();
    for resp in &ms.responses {
        let Ok(url) = r.url.join(&resp.href) else {
            tracing::warn!(href = %resp.href, "DAV collection href unreadable; skipped");
            continue;
        };
        let key = pct_decode(url.path());
        if same_path(&key, &home_key) {
            continue;
        }
        if !allowed(&url, s.origins) {
            let url = scrub(url);
            tracing::warn!(%url, "DAV collection outside the allowed origins; skipped");
            continue;
        }
        let rt = resp.prop(DAV, "resourcetype");
        let has = |ns: &str, n: &str| rt.is_some_and(|r| r.kid(ns, n).is_some());
        if has(CALDAV, "schedule-inbox")
            || has(CALDAV, "schedule-outbox")
            || has(CS, "notification")
            || has(CS, "subscribed")
        {
            continue;
        }
        let kind = if cal && has(CALDAV, "calendar") {
            match resp.prop(CALDAV, "supported-calendar-component-set") {
                None => EntityKind::Event,
                Some(set) => {
                    let comp = |c: &str| {
                        set.all(CALDAV, "comp")
                            .any(|k| k.attr("name").is_some_and(|n| n.eq_ignore_ascii_case(c)))
                    };
                    if comp("VEVENT") {
                        EntityKind::Event
                    } else if comp("VTODO") {
                        EntityKind::Task
                    } else {
                        continue;
                    }
                }
            }
        } else if !cal && has(CARDDAV, "addressbook") {
            EntityKind::Contact
        } else {
            continue;
        };
        let ctag = resp.text(CS, "getctag").is_some();
        let level = match overrides.get(&key) {
            Some(Level::None) => Level::None,
            Some(_) if ctag => Level::CtagEtag,
            Some(_) => Level::None,
            None if resp
                .prop(DAV, "supported-report-set")
                .is_some_and(|r| r.has(DAV, "sync-collection")) =>
            {
                Level::SyncToken
            }
            None if ctag => Level::CtagEtag,
            None => Level::None,
        };
        if let Some(o) = overrides.get_mut(&key) {
            *o = level;
        }
        let access = match resp.prop(DAV, "current-user-privilege-set") {
            None => Access::Owner,
            Some(p)
                if ["write-content", "write", "all"]
                    .iter()
                    .any(|w| p.has(DAV, w)) =>
            {
                Access::Writer
            }
            Some(_) => Access::Reader,
        };
        let name = resp
            .text(DAV, "displayname")
            .map(str::to_owned)
            .unwrap_or_else(|| {
                key.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            });
        let mut rc = RemoteCollection::new(CollectionRef(key.clone()), kind, name, access);
        rc.time_zone = resp.text(CALDAV, "calendar-timezone").and_then(tzid);
        rc.delta = Some(level.into());
        rc.push = Some(PushSupport::None);
        let col = Col {
            origin: origin(&url),
            kind,
            delta: level,
        };
        out.push((key, col, rc));
    }
    Ok(out)
}

/// The `TZID` of a `calendar-timezone` VTIMEZONE, when it is an IANA name.
fn tzid(vcalendar: &str) -> Option<String> {
    let id = vcalendar
        .lines()
        .find_map(|l| l.trim_end().strip_prefix("TZID:"))?
        .trim();
    jiff::tz::TimeZone::get(id).is_ok().then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn tzid_takes_known_zones_only() {
        let tz = "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Prague\r\nEND:VTIMEZONE\r\nEND:VCALENDAR\r\n";
        assert_eq!(super::tzid(tz).as_deref(), Some("Europe/Prague"));
        assert_eq!(super::tzid("TZID:Mars/Olympus\r\n"), None);
    }
}
