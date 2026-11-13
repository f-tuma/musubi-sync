//! WebDAV XML: a small namespace-aware tree, `multistatus` responses, request
//! bodies and href percent-coding.
//!
//! The parser rejects a DTD outright, so no entity is ever defined, let alone
//! fetched: only the predefined entities and character references decode.

use std::fmt::Write as _;

use quick_xml::NsReader;
use quick_xml::escape::{escape, resolve_predefined_entity};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;

pub(crate) const DAV: &str = "DAV:";
pub(crate) const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub(crate) const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub(crate) const CS: &str = "http://calendarserver.org/ns/";

/// One element: namespace, local name, text and child elements.
#[derive(Debug, Default, Clone)]
pub(crate) struct El {
    pub ns: String,
    pub name: String,
    /// Character data exactly as sent after entity decoding: no trimming and
    /// no end-of-line normalisation (`calendar-data` keeps its CRLF).
    pub text: String,
    pub attrs: Vec<(String, String)>,
    pub kids: Vec<El>,
}

impl El {
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    pub fn kid(&self, ns: &str, name: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.is(ns, name))
    }

    pub fn all<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a El> {
        self.kids.iter().filter(move |k| k.is(ns, name))
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether this element or a descendant is `{ns}name`.
    pub fn has(&self, ns: &str, name: &str) -> bool {
        self.is(ns, name) || self.kids.iter().any(|k| k.has(ns, name))
    }

    /// Text of the first `D:href` child, trimmed.
    pub fn href(&self) -> Option<&str> {
        self.kid(DAV, "href").map(|h| h.text.trim())
    }
}

/// Deepest element nesting [`parse`] accepts.
const MAX_DEPTH: usize = 64;

/// Parses an XML document into its root element.
pub(crate) fn parse(xml: &[u8]) -> Result<El, String> {
    let xml = std::str::from_utf8(xml).map_err(|e| e.to_string())?;
    let mut r = NsReader::from_str(xml);
    let mut stack = vec![El::default()];
    loop {
        let (res, ev) = r.read_resolved_event().map_err(|e| e.to_string())?;
        let ns = match res {
            ResolveResult::Bound(n) => n.as_ref().to_owned(),
            _ => String::new(),
        };
        let top = stack.len() - 1;
        // Drop, clone and `has` recurse per level: a deep tree overflows the stack.
        if matches!(ev, Event::Start(_) | Event::Empty(_)) && stack.len() > MAX_DEPTH {
            return Err("XML nested too deeply".into());
        }
        match ev {
            Event::Start(e) => stack.push(open(ns, &e)?),
            Event::Empty(e) => {
                let el = open(ns, &e)?;
                stack[top].kids.push(el);
            }
            Event::End(_) => {
                let el = stack.pop().ok_or("unbalanced end tag")?;
                stack.last_mut().ok_or("unbalanced end tag")?.kids.push(el);
            }
            Event::Text(t) => stack[top].text.push_str(&t),
            Event::CData(t) => stack[top].text.push_str(&t),
            Event::GeneralRef(g) => match g.resolve_char_ref().map_err(|e| e.to_string())? {
                Some(c) => stack[top].text.push(c),
                None => {
                    let v = resolve_predefined_entity(&g).ok_or("undefined entity")?;
                    stack[top].text.push_str(v);
                }
            },
            Event::DocType(_) => return Err("DTD not allowed".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    let mut root = stack
        .pop()
        .filter(|_| stack.is_empty())
        .ok_or("unclosed element")?;
    root.kids
        .pop()
        .filter(|_| root.kids.is_empty())
        .ok_or_else(|| "expected one root element".into())
}

fn open(ns: String, e: &BytesStart<'_>) -> Result<El, String> {
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| e.to_string())?;
        let v = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| e.to_string())?;
        let key = a.key.local_name().as_ref().to_owned();
        attrs.push((key, v.into_owned()));
    }
    Ok(El {
        ns,
        name: e.local_name().as_ref().to_owned(),
        attrs,
        ..El::default()
    })
}

/// One `D:response` of a multistatus.
#[derive(Debug)]
pub(crate) struct Response {
    /// The `D:href` as sent (still percent-encoded, maybe an absolute URL).
    pub href: String,
    /// Status of a response without propstat (`404` of a removed member,
    /// `507` of a truncated `sync-collection`).
    pub status: Option<u16>,
    /// Properties of the `200` propstat.
    pub props: Vec<El>,
}

impl Response {
    pub fn prop(&self, ns: &str, name: &str) -> Option<&El> {
        self.props.iter().find(|p| p.is(ns, name))
    }

    /// Trimmed text of a property; `None` when absent or empty.
    pub fn text(&self, ns: &str, name: &str) -> Option<&str> {
        self.prop(ns, name)
            .map(|p| p.text.trim())
            .filter(|t| !t.is_empty())
    }
}

/// A `207 Multi-Status` body.
#[derive(Debug)]
pub(crate) struct Multistatus {
    pub responses: Vec<Response>,
    /// `D:sync-token` of a `sync-collection` report.
    pub sync_token: Option<String>,
}

pub(crate) fn multistatus(body: &[u8]) -> Result<Multistatus, String> {
    let root = parse(body)?;
    if !root.is(DAV, "multistatus") {
        return Err(format!("expected D:multistatus, got {}", root.name));
    }
    let mut responses = Vec::new();
    for r in root.all(DAV, "response") {
        let href = r.href().ok_or("D:response without D:href")?.to_owned();
        let status = r.kid(DAV, "status").and_then(|s| status_code(&s.text));
        let mut props = Vec::new();
        for ps in r.all(DAV, "propstat") {
            let ok = ps
                .kid(DAV, "status")
                .and_then(|s| status_code(&s.text))
                .is_some_and(|c| (200..300).contains(&c));
            if let (true, Some(p)) = (ok, ps.kid(DAV, "prop")) {
                props.extend(p.kids.iter().cloned());
            }
        }
        responses.push(Response {
            href,
            status,
            props,
        });
    }
    let sync_token = root
        .kid(DAV, "sync-token")
        .map(|t| t.text.trim().to_owned());
    Ok(Multistatus {
        responses,
        sync_token,
    })
}

/// `HTTP/1.1 404 Not Found` → `404`.
fn status_code(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Whether an error body is `<D:error>` with the given precondition.
pub(crate) fn has_condition(body: &[u8], ns: &str, name: &str) -> bool {
    parse(body).is_ok_and(|e| e.is(DAV, "error") && e.kid(ns, name).is_some())
}

/// Percent-decodes a path; a `%` not followed by two hex digits stays as is.
pub(crate) fn pct_decode(path: &str) -> String {
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = path
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes everything but unreserved characters and `/`, as Radicale
/// does (`a@b c.ics` → `a%40b%20c.ics`).
pub(crate) fn pct_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &c in path.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-._~/".contains(&c) {
            out.push(c as char);
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
    out
}

const NS_DECL: &str = r#"xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CR="urn:ietf:params:xml:ns:carddav" xmlns:CS="http://calendarserver.org/ns/""#;

/// `PROPFIND` body for the given properties (`"D:getetag"`, …).
pub(crate) fn propfind(props: &[&str]) -> String {
    let mut b = format!(r#"<?xml version="1.0" encoding="utf-8"?><D:propfind {NS_DECL}><D:prop>"#);
    for p in props {
        let _ = write!(b, "<{p}/>");
    }
    b + "</D:prop></D:propfind>"
}

/// `sync-collection` report; an empty token starts an initial read.
pub(crate) fn sync_collection(token: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:sync-collection {NS_DECL}><D:sync-token>{}</D:sync-token><D:sync-level>1</D:sync-level><D:prop><D:getetag/></D:prop></D:sync-collection>"#,
        escape(token)
    )
}

/// `calendar-query` for events overlapping `[start, end)` (UTC, `YYYYMMDDTHHMMSSZ`).
pub(crate) fn calendar_query(start: &str, end: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><C:calendar-query {NS_DECL}><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="{start}" end="{end}"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#
    )
}

/// `calendar-multiget` or `addressbook-multiget` of decoded paths.
pub(crate) fn multiget(card: bool, paths: &[&str]) -> String {
    let (report, data) = if card {
        ("CR:addressbook-multiget", "CR:address-data")
    } else {
        ("C:calendar-multiget", "C:calendar-data")
    };
    let mut b = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><{report} {NS_DECL}><D:prop><D:getetag/><{data}/></D:prop>"#
    );
    for p in paths {
        let _ = write!(b, "<D:href>{}</D:href>", escape(pct_encode(p)));
    }
    let _ = write!(b, "</{report}>");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_crlf_and_decodes_entities() {
        let el = parse(
            b"<D:multistatus xmlns:D=\"DAV:\"><D:x>A\r\nB &amp; &#67;<![CDATA[<d>]]></D:x></D:multistatus>",
        )
        .unwrap();
        assert_eq!(el.kids[0].text, "A\r\nB & C<d>");
    }

    #[test]
    fn rejects_dtd_and_undefined_entities() {
        let xxe = br#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY e SYSTEM "file:///etc/passwd">]><x>&e;</x>"#;
        assert!(parse(xxe).unwrap_err().contains("DTD"));
        assert!(parse(b"<x>&e;</x>").is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        let deep = format!("{}{}", "<a>".repeat(100), "</a>".repeat(100));
        assert_eq!(parse(deep.as_bytes()).unwrap_err(), "XML nested too deeply");
        let empty = format!("{}<b/>{}", "<a>".repeat(64), "</a>".repeat(64));
        assert!(parse(empty.as_bytes()).is_err());
        let ok = format!("{}{}", "<a>".repeat(64), "</a>".repeat(64));
        assert!(parse(ok.as_bytes()).is_ok());
    }

    #[test]
    fn multistatus_statuses_and_props() {
        let ms = multistatus(
            br#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/c/a%40b%20c.ics</D:href><D:propstat><D:prop><D:getetag>"1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><D:displayname/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response><D:response><D:href>/c/</D:href><D:status>HTTP/1.1 507 Insufficient Storage</D:status></D:response><D:sync-token>t2</D:sync-token></D:multistatus>"#,
        )
        .unwrap();
        assert_eq!(ms.responses[0].text(DAV, "getetag"), Some("\"1\""));
        assert!(ms.responses[0].prop(DAV, "displayname").is_none());
        assert_eq!(ms.responses[1].status, Some(507));
        assert_eq!(ms.sync_token.as_deref(), Some("t2"));
    }

    #[test]
    fn percent_coding_round_trips() {
        let p = "/c/a@b c.ics";
        assert_eq!(pct_encode(p), "/c/a%40b%20c.ics");
        assert_eq!(pct_decode(&pct_encode(p)), p);
        assert_eq!(pct_decode("/c/%zz"), "/c/%zz");
    }
}
