//! Behaviour and response shapes of the fake DAV server (plan 8.2, K3): every
//! method, every `Fault` and `Quirk`, both profiles and the request counter.

use std::time::{Duration, Instant};

use musubi_testkit::dav::{
    Config, El, FakeDav, Fault, Host, Kind, NS_CALDAV, NS_CARDDAV, NS_CS, NS_DAV, Profile, Quirk,
    Req, SYNC_TOKEN_PREFIX, parse,
};
use reqwest::header::HeaderMap;

const ALICE: Option<(&str, &str)> = Some(("alice", "secret"));

struct Resp {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl Resp {
    fn xml(&self) -> El {
        parse(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().expect("ascii"))
    }
}

async fn send(
    method: &str,
    url: &str,
    depth: Option<&str>,
    body: &str,
    auth: Option<(&str, &str)>,
) -> Resp {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let mut r = http
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
            url,
        )
        .body(body.to_owned());
    if let Some((u, p)) = auth {
        r = r.basic_auth(u, Some(p));
    }
    if let Some(d) = depth {
        r = r.header("depth", d);
    }
    if !body.is_empty() {
        r = r.header("content-type", "application/xml; charset=utf-8");
    }
    let resp = r.send().await.expect("send");
    Resp {
        status: resp.status().as_u16(),
        headers: resp.headers().clone(),
        body: resp.text().await.expect("body"),
    }
}

const NS: &str = r#"xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CR="urn:ietf:params:xml:ns:carddav" xmlns:CS="http://calendarserver.org/ns/""#;

fn propfind(props: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:propfind {NS}><D:prop>{props}</D:prop></D:propfind>"#
    )
}

fn sync_collection(token: &str, limit: Option<usize>) -> String {
    let limit = limit.map_or(String::new(), |n| {
        format!("<D:limit><D:nresults>{n}</D:nresults></D:limit>")
    });
    format!(
        r#"<?xml version="1.0"?><D:sync-collection {NS}><D:sync-token>{token}</D:sync-token><D:sync-level>1</D:sync-level>{limit}<D:prop><D:getetag/></D:prop></D:sync-collection>"#
    )
}

fn multiget(root: &str, hrefs: &[&str], data: &str) -> String {
    let hrefs: String = hrefs
        .iter()
        .map(|h| format!("<D:href>{h}</D:href>"))
        .collect();
    format!(
        r#"<?xml version="1.0"?><{root} {NS}><D:prop><D:getetag/><{data}/></D:prop>{hrefs}</{root}>"#
    )
}

async fn pf(dav: &FakeDav, host: Host, path: &str, depth: &str, props: &str) -> El {
    let r = send(
        "PROPFIND",
        &format!("{}{path}", dav.url(host)),
        Some(depth),
        &propfind(props),
        ALICE,
    )
    .await;
    assert_eq!(r.status, 207, "{}", r.body);
    assert_eq!(
        r.header("content-type"),
        Some("application/xml; charset=utf-8")
    );
    r.xml()
}

async fn report(dav: &FakeDav, path: &str, body: &str) -> Resp {
    send(
        "REPORT",
        &format!("{}{path}", dav.url(Host::Caldav)),
        Some("1"),
        body,
        ALICE,
    )
    .await
}

/// The `D:response` for `href`.
fn resp<'a>(ms: &'a El, href: &str) -> &'a El {
    assert!(ms.is(NS_DAV, "multistatus"), "{ms:?}");
    ms.all(NS_DAV, "response")
        .find(|r| r.kid(NS_DAV, "href").is_some_and(|h| h.text == href))
        .unwrap_or_else(|| panic!("no response for {href}: {ms:?}"))
}

fn hrefs(ms: &El) -> Vec<String> {
    ms.all(NS_DAV, "response")
        .map(|r| r.kid(NS_DAV, "href").expect("href").text.clone())
        .collect()
}

/// A property in the response's `200` propstat.
fn found<'a>(r: &'a El, ns: &str, name: &str) -> Option<&'a El> {
    r.all(NS_DAV, "propstat")
        .filter(|p| {
            p.kid(NS_DAV, "status")
                .is_some_and(|s| s.text == "HTTP/1.1 200 OK")
        })
        .find_map(|p| p.kid(NS_DAV, "prop")?.kid(ns, name))
}

/// The property is in the response's `404` propstat.
fn missing(r: &El, ns: &str, name: &str) -> bool {
    r.all(NS_DAV, "propstat")
        .filter(|p| {
            p.kid(NS_DAV, "status")
                .is_some_and(|s| s.text == "HTTP/1.1 404 Not Found")
        })
        .any(|p| {
            p.kid(NS_DAV, "prop")
                .and_then(|p| p.kid(ns, name))
                .is_some()
        })
}

fn status_of(r: &El) -> Option<&str> {
    r.kid(NS_DAV, "status").map(|s| s.text.as_str())
}

fn href_in<'a>(r: &'a El, ns: &str, name: &str) -> &'a str {
    &found(r, ns, name)
        .and_then(|p| p.kid(NS_DAV, "href"))
        .unwrap_or_else(|| panic!("no {name} href: {r:?}"))
        .text
}

fn token(ms: &El) -> String {
    ms.kid(NS_DAV, "sync-token")
        .expect("sync-token")
        .text
        .clone()
}

fn error_condition(r: &Resp) -> String {
    let e = r.xml();
    assert!(e.is(NS_DAV, "error"), "{}", r.body);
    assert_eq!(
        r.header("content-type"),
        Some("application/xml; charset=utf-8")
    );
    let kid = e.kids.first().expect("condition");
    assert_eq!(kid.ns, NS_DAV);
    kid.name.clone()
}

fn ics(uid: &str, comp: &str, start: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//test//EN\r\nBEGIN:{comp}\r\nUID:{uid}\r\n\
         DTSTAMP:20261101T000000Z\r\nDTSTART:{start}\r\nSUMMARY:{uid}\r\nEND:{comp}\r\nEND:VCALENDAR\r\n"
    )
}

fn vcard(version: &str, name: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:{version}\r\nUID:{name}\r\nFN:{name}\r\nEND:VCARD\r\n")
}

async fn start(profile: Profile, split: bool) -> FakeDav {
    FakeDav::start(Config {
        profile,
        split,
        ..Config::default()
    })
    .await
    .expect("start")
}

async fn generic() -> (FakeDav, String) {
    let dav = start(Profile::Generic, false).await;
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT", "VTODO"]), "work", "Work & play");
    (dav, cal)
}

#[tokio::test]
async fn well_known_redirects_unauthenticated_and_the_rest_needs_basic_auth() {
    let dav = start(Profile::Generic, false).await;
    for wk in ["/.well-known/caldav", "/.well-known/carddav"] {
        let r = send(
            "GET",
            &format!("{}{wk}", dav.url(Host::Caldav)),
            None,
            "",
            None,
        )
        .await;
        assert_eq!((r.status, r.header("location")), (301, Some("/")), "{wk}");
    }
    let url = dav.server_url();
    let body = propfind("<D:current-user-principal/>");
    for auth in [None, Some(("alice", "wrong")), Some(("bob", "secret"))] {
        let r = send("PROPFIND", &url, Some("0"), &body, auth).await;
        assert_eq!(r.status, 401, "{auth:?}");
        assert_eq!(
            r.header("www-authenticate"),
            Some("Basic realm=\"musubi-fake-dav\"")
        );
    }
    let r = send("PROPFIND", &url, Some("0"), &body, ALICE).await;
    assert_eq!(r.status, 207);
    assert_eq!(
        href_in(resp(&r.xml(), "/"), NS_DAV, "current-user-principal"),
        "/principals/alice/"
    );

    dav.set_password("rotated");
    assert_eq!(
        send("PROPFIND", &url, Some("0"), &body, ALICE).await.status,
        401
    );
    let r = send(
        "PROPFIND",
        &url,
        Some("0"),
        &body,
        Some(("alice", "rotated")),
    )
    .await;
    assert_eq!(r.status, 207);
}

#[tokio::test]
async fn principal_and_homes_answer_discovery_propfinds() {
    let (dav, cal) = generic().await;
    let book = dav.add_collection(Kind::AddressBook, "contacts", "Contacts");
    let sub = dav.add_collection(Kind::Subscribed, "holidays", "Holidays");
    let all = dav.add_collection(Kind::Calendar(&[]), "all", "All");
    assert_eq!(
        (
            cal.as_str(),
            book.as_str(),
            dav.principal(),
            dav.calendar_home(),
            dav.addressbook_home()
        ),
        (
            "/calendars/alice/work/",
            "/addressbooks/alice/contacts/",
            "/principals/alice/".into(),
            "/calendars/alice/".into(),
            "/addressbooks/alice/".into()
        )
    );

    let ms = pf(
        &dav,
        Host::Caldav,
        "/principals/alice/",
        "0",
        "<D:principal-URL/><C:calendar-home-set/><CR:addressbook-home-set/><D:displayname/><D:resourcetype/><X:nope xmlns:X=\"urn:x\"/>",
    )
    .await;
    let p = resp(&ms, "/principals/alice/");
    assert_eq!(href_in(p, NS_DAV, "principal-URL"), "/principals/alice/");
    assert_eq!(
        href_in(p, NS_CALDAV, "calendar-home-set"),
        "/calendars/alice/"
    );
    assert_eq!(
        href_in(p, NS_CARDDAV, "addressbook-home-set"),
        "/addressbooks/alice/"
    );
    assert_eq!(found(p, NS_DAV, "displayname").expect("name").text, "alice");
    assert!(
        found(p, NS_DAV, "resourcetype")
            .expect("rt")
            .kid(NS_DAV, "principal")
            .is_some()
    );
    assert!(
        missing(p, "urn:x", "nope"),
        "unknown props go to a 404 propstat: {p:?}"
    );

    let props = "<D:resourcetype/><D:displayname/><C:supported-calendar-component-set/><D:sync-token/>\
                 <CS:getctag/><D:supported-report-set/><D:current-user-privilege-set/>";
    let ms = pf(&dav, Host::Caldav, "/calendars/alice/", "1", props).await;
    let mut got = hrefs(&ms);
    got.sort();
    assert_eq!(
        got,
        [
            "/calendars/alice/",
            "/calendars/alice/all/",
            "/calendars/alice/holidays/",
            "/calendars/alice/inbox/",
            "/calendars/alice/outbox/",
            "/calendars/alice/work/"
        ]
    );
    let w = resp(&ms, &cal);
    let rt = found(w, NS_DAV, "resourcetype").expect("rt");
    assert!(rt.kid(NS_DAV, "collection").is_some() && rt.kid(NS_CALDAV, "calendar").is_some());
    assert_eq!(
        found(w, NS_DAV, "displayname").expect("name").text,
        "Work & play"
    );
    let comps: Vec<_> = found(w, NS_CALDAV, "supported-calendar-component-set")
        .expect("comps")
        .all(NS_CALDAV, "comp")
        .map(|c| c.attr("name").expect("name").to_owned())
        .collect();
    assert_eq!(comps, ["VEVENT", "VTODO"]);
    assert!(
        found(w, NS_DAV, "sync-token")
            .expect("token")
            .text
            .starts_with(&format!("{SYNC_TOKEN_PREFIX}{cal}"))
    );
    assert!(
        found(w, NS_CS, "getctag")
            .expect("ctag")
            .text
            .starts_with("ctag-")
    );
    let reports: Vec<_> = found(w, NS_DAV, "supported-report-set")
        .expect("reports")
        .all(NS_DAV, "supported-report")
        .map(|r| {
            r.kid(NS_DAV, "report").expect("report").kids[0]
                .name
                .clone()
        })
        .collect();
    assert_eq!(
        reports,
        ["calendar-query", "calendar-multiget", "sync-collection"]
    );
    let privs: Vec<_> = found(w, NS_DAV, "current-user-privilege-set")
        .expect("privs")
        .all(NS_DAV, "privilege")
        .map(|p| p.kids[0].name.clone())
        .collect();
    assert!(
        privs.contains(&"write".to_owned()) && privs.contains(&"read".to_owned()),
        "{privs:?}"
    );
    assert!(missing(
        resp(&ms, &all),
        NS_CALDAV,
        "supported-calendar-component-set"
    ));
    for (href, ns, name) in [
        ("/calendars/alice/inbox/", NS_CALDAV, "schedule-inbox"),
        ("/calendars/alice/outbox/", NS_CALDAV, "schedule-outbox"),
        (sub.as_str(), NS_CS, "subscribed"),
    ] {
        let r = resp(&ms, href);
        assert!(
            found(r, NS_DAV, "resourcetype")
                .expect("rt")
                .kid(ns, name)
                .is_some(),
            "{href}"
        );
        assert!(missing(r, NS_DAV, "sync-token"), "{href} does not sync");
    }

    let ms = pf(
        &dav,
        Host::Caldav,
        "/addressbooks/alice/",
        "1",
        "<D:resourcetype/><CR:supported-address-data/>",
    )
    .await;
    let b = resp(&ms, &book);
    assert!(
        found(b, NS_DAV, "resourcetype")
            .expect("rt")
            .kid(NS_CARDDAV, "addressbook")
            .is_some()
    );
    let versions: Vec<_> = found(b, NS_CARDDAV, "supported-address-data")
        .expect("data")
        .all(NS_CARDDAV, "address-data-type")
        .map(|t| {
            (
                t.attr("content-type").expect("ct").to_owned(),
                t.attr("version").expect("v").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        versions,
        [
            ("text/vcard".into(), "3.0".into()),
            ("text/vcard".into(), "4.0".into())
        ]
    );

    dav.remove_collection(&book);
    let r = send(
        "PROPFIND",
        &format!("{}{book}", dav.url(Host::Caldav)),
        Some("0"),
        &propfind("<D:resourcetype/>"),
        ALICE,
    )
    .await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn objects_list_with_etags_and_get_head_options_serve_them() {
    let (dav, cal) = generic().await;
    let body = ics("a", "VEVENT", "20261201T100000Z");
    let path = dav.put(&cal, "a.ics", &body);
    assert_eq!(path, format!("{cal}a.ics"));
    let etag = dav.etag(&path).expect("etag");
    assert!(etag.starts_with("\"e") && etag.ends_with('"'), "{etag}");

    let ms = pf(
        &dav,
        Host::Caldav,
        &cal,
        "1",
        "<D:getetag/><D:getcontenttype/><D:resourcetype/>",
    )
    .await;
    assert_eq!(hrefs(&ms), [cal.clone(), path.clone()]);
    let o = resp(&ms, &path);
    assert_eq!(found(o, NS_DAV, "getetag").expect("etag").text, etag);
    assert_eq!(
        found(o, NS_DAV, "getcontenttype").expect("ct").text,
        "text/calendar; charset=utf-8"
    );
    assert!(
        found(o, NS_DAV, "resourcetype")
            .expect("rt")
            .kids
            .is_empty()
    );

    // allprop (empty body).
    let r = send(
        "PROPFIND",
        &format!("{}{cal}", dav.url(Host::Caldav)),
        Some("0"),
        "",
        ALICE,
    )
    .await;
    assert_eq!(r.status, 207);
    let c = resp(&r.xml(), &cal).to_owned_names();
    assert_eq!(c, ["resourcetype", "displayname"]);

    let url = format!("{}{path}", dav.url(Host::Caldav));
    let r = send("GET", &url, None, "", ALICE).await;
    assert_eq!(
        (r.status, r.body.as_str(), r.header("etag")),
        (200, body.as_str(), Some(etag.as_str()))
    );
    assert_eq!(
        r.header("content-type"),
        Some("text/calendar; charset=utf-8")
    );
    let r = send("HEAD", &url, None, "", ALICE).await;
    assert_eq!(
        (r.status, r.body.as_str(), r.header("etag")),
        (200, "", Some(etag.as_str()))
    );

    let r = send("OPTIONS", &dav.server_url(), None, "", ALICE).await;
    assert_eq!(r.status, 200);
    assert!(r.header("dav").expect("dav").contains("calendar-access"));
    assert!(r.header("dav").expect("dav").contains("addressbook"));
    assert_eq!(
        r.header("allow"),
        Some("OPTIONS, GET, HEAD, PROPFIND, REPORT")
    );

    assert_eq!(
        send(
            "GET",
            &format!("{}{cal}", dav.url(Host::Caldav)),
            None,
            "",
            ALICE
        )
        .await
        .status,
        405
    );
    assert_eq!(
        send("GET", &format!("{url}x"), None, "", ALICE)
            .await
            .status,
        404
    );
    for m in ["PUT", "DELETE", "MKCOL", "PROPPATCH", "MOVE"] {
        assert_eq!(send(m, &url, None, "", ALICE).await.status, 405, "{m}");
    }
    let r = send("PROPFIND", &url, Some("0"), "<not-xml", ALICE).await;
    assert_eq!(r.status, 400);

    dav.delete(&path);
    assert_eq!(send("GET", &url, None, "", ALICE).await.status, 404);
}

trait Names {
    fn to_owned_names(&self) -> Vec<String>;
}

impl Names for El {
    /// Names of the `200` propstat's properties.
    fn to_owned_names(&self) -> Vec<String> {
        self.all(NS_DAV, "propstat")
            .flat_map(|p| {
                p.kid(NS_DAV, "prop")
                    .into_iter()
                    .flat_map(|p| p.kids.iter())
            })
            .map(|k| k.name.clone())
            .collect()
    }
}

#[tokio::test]
async fn sync_collection_reads_deltas_with_tombstones_and_pages_by_limit() {
    let (dav, cal) = generic().await;
    let a = dav.put(&cal, "a.ics", &ics("a", "VEVENT", "20261201T100000Z"));
    let b = dav.put(&cal, "b.ics", &ics("b", "VEVENT", "20261202T100000Z"));
    let c = dav.put(&cal, "c.ics", &ics("c", "VEVENT", "20261203T100000Z"));
    dav.delete(&c);

    let r = report(&dav, &cal, &sync_collection("", None)).await;
    assert_eq!(r.status, 207, "{}", r.body);
    let ms = r.xml();
    assert_eq!(
        hrefs(&ms),
        [a.clone(), b.clone()],
        "an initial read lists live members only"
    );
    assert_eq!(
        found(resp(&ms, &a), NS_DAV, "getetag").expect("etag").text,
        dav.etag(&a).expect("etag")
    );
    let t0 = token(&ms);
    assert_eq!(t0, dav.sync_token(&cal));

    let r = report(&dav, &cal, &sync_collection(&t0, None)).await;
    assert!(hrefs(&r.xml()).is_empty(), "no change, no members");
    assert_eq!(token(&r.xml()), t0);

    dav.put(&cal, "a.ics", &ics("a", "VEVENT", "20261201T110000Z"));
    dav.delete(&b);
    let d = dav.put(&cal, "d.ics", &ics("d", "VTODO", "20261204T100000Z"));
    let ms = report(&dav, &cal, &sync_collection(&t0, None)).await.xml();
    assert_eq!(hrefs(&ms), [a.clone(), b.clone(), d.clone()]);
    assert!(found(resp(&ms, &a), NS_DAV, "getetag").is_some());
    assert_eq!(status_of(resp(&ms, &b)), Some("HTTP/1.1 404 Not Found"));
    let t1 = token(&ms);
    assert_ne!(t1, t0);

    // DAV:limit pages: 507 for the collection and a token that continues.
    let e = dav.put(&cal, "e.ics", &ics("e", "VEVENT", "20261207T100000Z"));
    dav.put(&cal, "e.ics", &ics("e", "VEVENT", "20261207T110000Z"));
    let f = dav.put(&cal, "f.ics", &ics("f", "VEVENT", "20261208T100000Z"));
    let g = dav.put(&cal, "g.ics", &ics("g", "VEVENT", "20261209T100000Z"));
    let ms = report(&dav, &cal, &sync_collection(&t1, Some(2)))
        .await
        .xml();
    assert_eq!(
        hrefs(&ms),
        [e, f, cal.clone()],
        "two members, then the collection's 507"
    );
    assert_eq!(
        status_of(resp(&ms, &cal)),
        Some("HTTP/1.1 507 Insufficient Storage")
    );
    let ms = report(&dav, &cal, &sync_collection(&token(&ms), Some(2)))
        .await
        .xml();
    assert_eq!(hrefs(&ms), [g], "the rest, not truncated");
    assert_eq!(token(&ms), dav.sync_token(&cal));

    // Expired, foreign and garbage tokens: 403 valid-sync-token.
    dav.expire_sync_tokens(&cal);
    for bad in [
        t1.clone(),
        format!("{SYNC_TOKEN_PREFIX}/calendars/alice/inbox/1"),
        "garbage".into(),
    ] {
        let r = report(&dav, &cal, &sync_collection(&bad, None)).await;
        assert_eq!(r.status, 403, "{bad}");
        assert_eq!(error_condition(&r), "valid-sync-token");
    }
    let r = report(&dav, &cal, &sync_collection("", None)).await;
    assert_eq!(r.status, 207, "a full read still works");
}

#[tokio::test]
async fn calendar_query_filters_by_component_and_time_range() {
    let (dav, cal) = generic().await;
    let inside = dav.put(&cal, "in.ics", &ics("in", "VEVENT", "20261201T100000Z"));
    dav.put(&cal, "out.ics", &ics("out", "VEVENT", "20270601T100000Z"));
    let task = dav.put(&cal, "t.ics", &ics("t", "VTODO", "20261201T100000Z"));
    let q = |comp: &str, range: &str| {
        format!(
            r#"<?xml version="1.0"?><C:calendar-query {NS}><D:prop><D:getetag/><C:calendar-data/></D:prop>
               <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="{comp}">{range}</C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#
        )
    };
    let range = r#"<C:time-range start="20261101T000000Z" end="20270101T000000Z"/>"#;
    let r = report(&dav, &cal, &q("VEVENT", range)).await;
    assert_eq!(r.status, 207, "{}", r.body);
    let ms = r.xml();
    assert_eq!(hrefs(&ms), std::slice::from_ref(&inside));
    assert_eq!(
        found(resp(&ms, &inside), NS_CALDAV, "calendar-data")
            .expect("data")
            .text,
        ics("in", "VEVENT", "20261201T100000Z")
    );
    assert_eq!(
        hrefs(&report(&dav, &cal, &q("VEVENT", "")).await.xml()).len(),
        2
    );
    assert_eq!(
        hrefs(&report(&dav, &cal, &q("VTODO", "")).await.xml()),
        [task]
    );
}

#[tokio::test]
async fn multigets_return_data_by_href_and_404_for_missing_ones() {
    let (dav, cal) = generic().await;
    let book = dav.add_collection(Kind::AddressBook, "contacts", "Contacts");
    let a = dav.put(&cal, "a.ics", &ics("a", "VEVENT", "20261201T100000Z"));
    let card = dav.put(&book, "x.vcf", &vcard("4.0", "X"));
    let absolute = format!("{}{a}", dav.url(Host::Caldav));
    let gone = format!("{cal}gone.ics");
    let r = report(
        &dav,
        &cal,
        &multiget(
            "C:calendar-multiget",
            &[&absolute, &gone],
            "C:calendar-data",
        ),
    )
    .await;
    assert_eq!(r.status, 207, "{}", r.body);
    let ms = r.xml();
    assert_eq!(
        hrefs(&ms),
        [a.clone(), gone.clone()],
        "absolute hrefs come back as paths"
    );
    let ra = resp(&ms, &a);
    assert!(
        found(ra, NS_CALDAV, "calendar-data")
            .expect("data")
            .text
            .contains("UID:a")
    );
    assert_eq!(
        found(ra, NS_DAV, "getetag").expect("etag").text,
        dav.etag(&a).expect("etag")
    );
    assert_eq!(status_of(resp(&ms, &gone)), Some("HTTP/1.1 404 Not Found"));

    let r = report(
        &dav,
        &book,
        &multiget("CR:addressbook-multiget", &[&card], "CR:address-data"),
    )
    .await;
    let rc = r.xml();
    assert_eq!(
        found(resp(&rc, &card), NS_CARDDAV, "address-data")
            .expect("data")
            .text,
        vcard("4.0", "X")
    );

    // A report the resource does not support: 403 supported-report.
    for (path, body) in [
        (
            book.as_str(),
            multiget("C:calendar-multiget", &[&card], "C:calendar-data"),
        ),
        ("/calendars/alice/", sync_collection("", None)),
        ("/calendars/alice/inbox/", sync_collection("", None)),
    ] {
        let r = report(&dav, path, &body).await;
        assert_eq!(r.status, 403, "{path}");
        assert_eq!(error_condition(&r), "supported-report", "{path}");
    }
    assert_eq!(
        report(&dav, "/calendars/alice/nope/", &sync_collection("", None))
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn status_faults_carry_retry_after_and_www_authenticate_then_wear_off() {
    let (dav, cal) = generic().await;
    let url = format!("{}{cal}", dav.url(Host::Caldav));
    let body = propfind("<D:getetag/>");
    dav.fail(
        Req::Any,
        Fault::Status {
            status: 429,
            retry_after: Some(3),
        },
        1,
    );
    let r = send("PROPFIND", &url, Some("0"), &body, ALICE).await;
    assert_eq!(
        (r.status, r.header("retry-after"), r.body.as_str()),
        (429, Some("3"), "")
    );
    assert_eq!(
        send("PROPFIND", &url, Some("0"), &body, ALICE).await.status,
        207,
        "times = 1"
    );

    dav.fail(
        Req::Method("REPORT"),
        Fault::Status {
            status: 503,
            retry_after: Some(120),
        },
        2,
    );
    assert_eq!(
        send("PROPFIND", &url, Some("0"), &body, ALICE).await.status,
        207,
        "PROPFIND is not REPORT"
    );
    for _ in 0..2 {
        let r = report(&dav, &cal, &sync_collection("", None)).await;
        assert_eq!((r.status, r.header("retry-after")), (503, Some("120")));
    }
    assert_eq!(
        report(&dav, &cal, &sync_collection("", None)).await.status,
        207
    );

    dav.fail(
        Req::Report("sync-collection"),
        Fault::Status {
            status: 401,
            retry_after: None,
        },
        1,
    );
    let r = report(
        &dav,
        &cal,
        &multiget("C:calendar-multiget", &[], "C:calendar-data"),
    )
    .await;
    assert_eq!(r.status, 207, "another report");
    let r = report(&dav, &cal, &sync_collection("", None)).await;
    assert_eq!(
        (r.status, r.header("www-authenticate")),
        (401, Some("Basic realm=\"musubi-fake-dav\""))
    );
    assert_eq!(r.header("retry-after"), None);

    for code in [403, 404, 405, 500, 501] {
        dav.fail(
            Req::Any,
            Fault::Status {
                status: code,
                retry_after: None,
            },
            1,
        );
        assert_eq!(
            report(&dav, &cal, &sync_collection("", None)).await.status,
            code
        );
    }

    // A 404 on one collection only.
    let other = dav.add_collection(Kind::Calendar(&["VEVENT"]), "other", "Other");
    dav.fail(
        Req::Path(cal.trim_end_matches('/').to_owned()),
        Fault::Status {
            status: 404,
            retry_after: None,
        },
        5,
    );
    assert_eq!(
        report(&dav, &other, &sync_collection("", None))
            .await
            .status,
        207
    );
    assert_eq!(
        report(&dav, &cal, &sync_collection("", None)).await.status,
        404
    );
}

#[tokio::test]
async fn invalid_sync_token_fault_answers_valid_sync_token_whatever_the_token() {
    let (dav, cal) = generic().await;
    let t = dav.sync_token(&cal);
    for status in [403, 409] {
        dav.fail(
            Req::Report("sync-collection"),
            Fault::InvalidSyncToken { status },
            1,
        );
        let r = report(&dav, &cal, &sync_collection(&t, None)).await;
        assert_eq!(r.status, status);
        assert_eq!(error_condition(&r), "valid-sync-token");
    }
    assert_eq!(
        report(&dav, &cal, &sync_collection(&t, None)).await.status,
        207
    );
}

#[tokio::test]
async fn truncate_fault_ends_sync_collection_with_507_and_a_continuing_token() {
    let (dav, cal) = generic().await;
    let names: Vec<String> = (0..5)
        .map(|i| {
            dav.put(
                &cal,
                &format!("{i}.ics"),
                &ics(&i.to_string(), "VEVENT", "20261201T100000Z"),
            )
        })
        .collect();
    dav.fail(Req::Any, Fault::Truncate(2), 2);
    // Does not apply to other requests (and is not consumed by them).
    pf(&dav, Host::Caldav, &cal, "1", "<D:getetag/>").await;
    let mut seen = Vec::new();
    let mut t = String::new();
    for round in 0..3 {
        let ms = report(&dav, &cal, &sync_collection(&t, None)).await.xml();
        let truncated = ms.all(NS_DAV, "response").any(|r| {
            status_of(r) == Some("HTTP/1.1 507 Insufficient Storage")
                && r.kid(NS_DAV, "href").is_some_and(|h| h.text == cal)
        });
        assert_eq!(truncated, round < 2, "round {round}");
        seen.extend(hrefs(&ms).into_iter().filter(|h| *h != cal));
        t = token(&ms);
    }
    assert_eq!(seen, names);
    assert_eq!(t, dav.sync_token(&cal));
}

#[tokio::test]
async fn delay_fault_and_latency_slow_responses_down() {
    let (dav, _) = generic().await;
    let url = dav.server_url();
    let body = propfind("<D:current-user-principal/>");
    dav.fail(Req::Any, Fault::Delay(Duration::from_millis(300)), 1);
    let t = Instant::now();
    assert_eq!(
        send("PROPFIND", &url, Some("0"), &body, ALICE).await.status,
        207
    );
    assert!(t.elapsed() >= Duration::from_millis(300));
    let t = Instant::now();
    send("PROPFIND", &url, Some("0"), &body, ALICE).await;
    assert!(t.elapsed() < Duration::from_millis(300), "once");
    dav.set_latency(Duration::from_millis(200));
    let t = Instant::now();
    send("PROPFIND", &url, Some("0"), &body, ALICE).await;
    assert!(t.elapsed() >= Duration::from_millis(200));
}

#[tokio::test]
async fn redirect_fault_points_at_the_partition_or_outside_and_outside_only_logs() {
    let (dav, cal) = generic().await;
    let path = format!("{cal}?x=1");
    dav.fail(
        Req::Any,
        Fault::Redirect {
            status: 301,
            to: Host::CaldavPartition,
        },
        1,
    );
    let r = send(
        "PROPFIND",
        &format!("{}{path}", dav.url(Host::Caldav)),
        Some("0"),
        &propfind("<D:resourcetype/>"),
        ALICE,
    )
    .await;
    let location = format!("{}{path}", dav.url(Host::CaldavPartition));
    assert_eq!(
        (r.status, r.header("location")),
        (301, Some(location.as_str()))
    );
    let r = send(
        "PROPFIND",
        &location,
        Some("0"),
        &propfind("<D:resourcetype/>"),
        ALICE,
    )
    .await;
    assert_eq!(r.status, 207, "the partition serves the same tree");

    for status in [302, 307, 308] {
        dav.fail(
            Req::Any,
            Fault::Redirect {
                status,
                to: Host::Outside,
            },
            1,
        );
        let r = send(
            "GET",
            &format!("{}/x", dav.url(Host::Caldav)),
            None,
            "",
            ALICE,
        )
        .await;
        let outside = format!("{}/x", dav.url(Host::Outside));
        assert_eq!(
            (r.status, r.header("location")),
            (status, Some(outside.as_str()))
        );
    }
    dav.clear_requests();
    let r = send(
        "PROPFIND",
        &format!("{}{cal}", dav.url(Host::Outside)),
        Some("0"),
        "",
        None,
    )
    .await;
    assert_eq!(r.status, 404, "outside answers 404 to everything");
    let log = dav.requests();
    assert_eq!(log.len(), 1);
    assert_eq!(
        (log[0].host, log[0].auth, log[0].method.as_str()),
        (Host::Outside, false, "PROPFIND")
    );
}

#[tokio::test]
async fn objects_change_or_vanish_between_listing_and_multiget() {
    let (dav, cal) = generic().await;
    let a = dav.put(&cal, "a.ics", &ics("a", "VEVENT", "20261201T100000Z"));
    let listed = dav.etag(&a).expect("etag");
    dav.fail(Req::Any, Fault::ChangeBeforeMultiget, 1);
    let ms = report(&dav, &cal, &sync_collection("", None)).await.xml();
    assert_eq!(
        found(resp(&ms, &a), NS_DAV, "getetag").expect("etag").text,
        listed,
        "not a multiget"
    );
    let t = token(&ms);
    let ms = report(
        &dav,
        &cal,
        &multiget("C:calendar-multiget", &[&a], "C:calendar-data"),
    )
    .await
    .xml();
    let fetched = found(resp(&ms, &a), NS_DAV, "getetag")
        .expect("etag")
        .text
        .clone();
    assert_ne!(fetched, listed);
    assert_eq!(dav.etag(&a), Some(fetched));
    assert_eq!(
        hrefs(&report(&dav, &cal, &sync_collection(&t, None)).await.xml()),
        std::slice::from_ref(&a),
        "the change shows in the next delta"
    );

    dav.fail(
        Req::Report("calendar-multiget"),
        Fault::DeleteBeforeMultiget,
        1,
    );
    let ms = report(
        &dav,
        &cal,
        &multiget("C:calendar-multiget", &[&a], "C:calendar-data"),
    )
    .await
    .xml();
    assert_eq!(status_of(resp(&ms, &a)), Some("HTTP/1.1 404 Not Found"));
    assert_eq!(dav.etag(&a), None);
    assert_eq!(
        send(
            "GET",
            &format!("{}{a}", dav.url(Host::Caldav)),
            None,
            "",
            ALICE
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn quirks_change_what_a_collection_reports() {
    let (dav, cal) = generic().await;
    let a = dav.put(&cal, "a.ics", &ics("a", "VEVENT", "20261201T100000Z"));
    let props =
        "<CS:getctag/><D:sync-token/><D:supported-report-set/><D:current-user-privilege-set/>";

    dav.quirk(&cal, Quirk::NoCtag);
    let c = pf(&dav, Host::Caldav, &cal, "0", props).await;
    assert!(missing(resp(&c, &cal), NS_CS, "getctag"));

    dav.quirk(&cal, Quirk::NoSync);
    let c = pf(&dav, Host::Caldav, &cal, "0", props).await;
    let r = resp(&c, &cal);
    assert!(missing(r, NS_DAV, "sync-token"));
    let reports = found(r, NS_DAV, "supported-report-set")
        .expect("reports")
        .all(NS_DAV, "supported-report")
        .count();
    assert_eq!(reports, 2, "no sync-collection");
    let resp_ = report(&dav, &cal, &sync_collection("", None)).await;
    assert_eq!(resp_.status, 403);
    assert_eq!(error_condition(&resp_), "supported-report");

    dav.quirk(&cal, Quirk::ReadOnly);
    let c = pf(&dav, Host::Caldav, &cal, "0", props).await;
    let privs: Vec<_> = found(resp(&c, &cal), NS_DAV, "current-user-privilege-set")
        .expect("privs")
        .all(NS_DAV, "privilege")
        .map(|p| p.kids[0].name.clone())
        .collect();
    assert_eq!(privs, ["read", "read-current-user-privilege-set"]);

    let url = format!("{}{a}", dav.url(Host::Caldav));
    dav.quirk(&cal, Quirk::WeakEtag);
    let weak = dav.etag(&a).expect("etag");
    assert!(weak.starts_with("W/\""), "{weak}");
    let c = pf(&dav, Host::Caldav, &cal, "1", "<D:getetag/>").await;
    assert_eq!(
        found(resp(&c, &a), NS_DAV, "getetag").expect("etag").text,
        weak
    );
    assert_eq!(
        send("GET", &url, None, "", ALICE).await.header("etag"),
        Some(weak.as_str())
    );

    dav.quirk(&cal, Quirk::NoEtag);
    assert_eq!(dav.etag(&a), None);
    let c = pf(&dav, Host::Caldav, &cal, "1", "<D:getetag/>").await;
    assert!(missing(resp(&c, &a), NS_DAV, "getetag"));
    let r = send("GET", &url, None, "", ALICE).await;
    assert_eq!((r.status, r.header("etag")), (200, None));
}

#[tokio::test]
async fn icloud_profile_splits_entries_and_puts_homes_on_partition_hosts() {
    let dav = start(Profile::Icloud, false).await;
    let book = dav.add_collection(Kind::AddressBook, "card", "Contacts");
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    assert_ne!(dav.carddav_url(), dav.server_url());
    let props = "<C:calendar-home-set/><CR:addressbook-home-set/>";

    let ms = pf(&dav, Host::Caldav, "/principals/alice/", "0", props).await;
    let p = resp(&ms, "/principals/alice/");
    assert_eq!(
        href_in(p, NS_CALDAV, "calendar-home-set"),
        format!("{}/calendars/alice/", dav.url(Host::CaldavPartition))
    );
    assert!(missing(p, NS_CARDDAV, "addressbook-home-set"));
    let ms = pf(&dav, Host::Carddav, "/principals/alice/", "0", props).await;
    let p = resp(&ms, "/principals/alice/");
    assert_eq!(
        href_in(p, NS_CARDDAV, "addressbook-home-set"),
        format!("{}/addressbooks/alice/", dav.url(Host::CarddavPartition))
    );
    assert!(missing(p, NS_CALDAV, "calendar-home-set"));

    let ms = pf(
        &dav,
        Host::CaldavPartition,
        "/calendars/alice/",
        "1",
        "<D:resourcetype/><D:current-user-privilege-set/>",
    )
    .await;
    let n = resp(&ms, "/calendars/alice/notification/");
    assert!(
        found(n, NS_DAV, "resourcetype")
            .expect("rt")
            .kid(NS_CS, "notification")
            .is_some()
    );
    assert!(
        missing(resp(&ms, &cal), NS_DAV, "current-user-privilege-set"),
        "iCloud leaves privileges out"
    );

    let ms = pf(
        &dav,
        Host::CarddavPartition,
        "/addressbooks/alice/",
        "1",
        "<CR:supported-address-data/>",
    )
    .await;
    let types = found(resp(&ms, &book), NS_CARDDAV, "supported-address-data")
        .expect("data")
        .all(NS_CARDDAV, "address-data-type")
        .count();
    assert_eq!(types, 1, "vCard 3.0 only");
    dav.put(&book, "a.vcf", &vcard("3.0", "A"));
}

#[tokio::test]
#[should_panic(expected = "iCloud serves vCard 3.0 only")]
async fn icloud_profile_refuses_vcard_4_fixtures() {
    let dav = start(Profile::Icloud, false).await;
    let book = dav.add_collection(Kind::AddressBook, "card", "Contacts");
    dav.put(&book, "a.vcf", &vcard("4.0", "A"));
}

#[tokio::test]
async fn generic_split_has_one_home_set_per_entry() {
    let dav = start(Profile::Generic, true).await;
    assert_eq!(dav.carddav_url(), format!("{}/", dav.url(Host::Carddav)));
    let props = "<C:calendar-home-set/><CR:addressbook-home-set/>";
    let p = pf(&dav, Host::Caldav, "/principals/alice/", "0", props).await;
    let p = resp(&p, "/principals/alice/");
    assert_eq!(
        href_in(p, NS_CALDAV, "calendar-home-set"),
        "/calendars/alice/"
    );
    assert!(missing(p, NS_CARDDAV, "addressbook-home-set"));
    let p = pf(&dav, Host::Carddav, "/principals/alice/", "0", props).await;
    let p = resp(&p, "/principals/alice/");
    assert_eq!(
        href_in(p, NS_CARDDAV, "addressbook-home-set"),
        "/addressbooks/alice/"
    );
    assert!(missing(p, NS_CALDAV, "calendar-home-set"));
}

#[tokio::test]
async fn requests_are_logged_and_counted_per_method_and_report() {
    let (dav, cal) = generic().await;
    pf(&dav, Host::Caldav, &cal, "1", "<D:getetag/>").await;
    report(&dav, &cal, &sync_collection("", None)).await;
    report(&dav, &cal, &sync_collection("", None)).await;
    report(
        &dav,
        &cal,
        &multiget("C:calendar-multiget", &[], "C:calendar-data"),
    )
    .await;
    send(
        "GET",
        &format!("{}/x", dav.url(Host::Carddav)),
        None,
        "",
        None,
    )
    .await;
    assert_eq!(dav.count("PROPFIND"), 1);
    assert_eq!(dav.count("REPORT"), 3);
    assert_eq!(dav.count("REPORT sync-collection"), 2);
    assert_eq!(dav.count("REPORT calendar-multiget"), 1);
    let log = dav.requests();
    assert_eq!(log[0].depth.as_deref(), Some("1"));
    assert_eq!(log[1].label(), "REPORT sync-collection");
    assert_eq!(
        (log[4].host, log[4].auth, log[4].path.as_str()),
        (Host::Carddav, false, "/x")
    );
    dav.clear_requests();
    assert!(dav.requests().is_empty());
}
