//! Outgoing requests: where the password may go (plan, chapter 5.5).
//!
//! - Every target, the first one and each redirect, must be in the account's
//!   allowed origins before anything is sent; a redirect elsewhere is never
//!   followed, so its target gets no request at all.
//! - The address a host name resolves to is checked by the client's own DNS
//!   resolver and the connection goes to exactly those addresses (no second
//!   lookup, no DNS rebinding). IP literals are checked here, as reqwest does
//!   not resolve them.
//! - `https` only, except `http` to loopback with `dav.allow_private`.
//! - No proxy from the environment: it would connect on our behalf and skip
//!   the address check.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use musubi_model::{AccountCx, Credential, SyncError};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_TYPE, HeaderMap, LOCATION, RETRY_AFTER};
use reqwest::{Method, Url};
use secrecy::{ExposeSecret, SecretString};

/// Redirects followed per request.
const MAX_REDIRECTS: usize = 5;
/// Response body cap.
const MAX_BODY: usize = 32 << 20;

/// The two HTTP clients and the address policy.
pub(crate) struct Net {
    https: reqwest::Client,
    /// Only with `allow_private`: `http` to loopback.
    http: Option<reqwest::Client>,
    allow_private: bool,
}

/// Why a request was not answered.
#[derive(Debug)]
pub(crate) enum NetError {
    /// The target address is not allowed (private, or `http` off loopback).
    Blocked(String),
    /// A redirect (or a target) outside the allowed origins, or from `https`
    /// to `http`: nothing was sent there.
    CrossOrigin(Url),
    /// `301`/`308` inside the allowed origins at run time.
    Moved,
    TooManyRedirects,
    TooLarge,
    Http(reqwest::Error),
}

/// A final (non-redirect) answer.
pub(crate) struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// Where it came from, after redirects: relative hrefs resolve against it.
    pub url: Url,
}

/// Redirect handling: discovery follows any redirect inside the allowed
/// origins; at run time a permanent one means the collection moved.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Discover,
    Run,
}

/// One account's view of the network: its credentials and allowed origins.
pub(crate) struct Session<'a> {
    pub net: &'a Net,
    pub cx: &'a AccountCx,
    user: &'a str,
    password: &'a SecretString,
    pub origins: &'a [String],
    pub mode: Mode,
}

/// `scheme://host[:port]`, lower case, without the scheme's default port.
pub(crate) fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Whether `url` may carry the password: inside the allowed origins and
/// without userinfo (which reqwest would send as a second `Authorization`).
pub(crate) fn allowed(url: &Url, origins: &[String]) -> bool {
    url.username().is_empty() && url.password().is_none() && origins.contains(&origin(url))
}

/// Whether a redirect from `from` to `to` may be followed: allowed, and never
/// from `https` down to anything else.
pub(crate) fn redirect_allowed(from: &Url, to: &Url, origins: &[String]) -> bool {
    (from.scheme() != "https" || to.scheme() == "https") && allowed(to, origins)
}

/// `url` without userinfo, for errors and logs.
pub(crate) fn scrub(mut url: Url) -> Url {
    // Err only for URLs that cannot have userinfo, which then have none.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url
}

/// Loopback, private, link-local, CGNAT, multicast, broadcast or unspecified,
/// also when embedded in IPv6 (IPv4-mapped, -compatible, SIIT, NAT64; 6to4,
/// local-use NAT64 and site-local count as private outright).
pub(crate) fn is_private(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(a) => {
            let [o0, o1, ..] = a.octets();
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_multicast()
                || a.is_broadcast()
                || a.is_unspecified()
                || o0 == 0
                || (o0 == 100 && (o1 & 0xc0) == 64)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            if matches!(
                s,
                [0, 0, 0, 0, 0, 0, ..]
                    | [0, 0, 0, 0, 0xffff, 0, ..]
                    | [0x64, 0xff9b, 0, 0, 0, 0, ..]
            ) {
                return is_private(IpAddr::V4(Ipv4Addr::from_bits(a.to_bits() as u32)));
            }
            // 64:ff9b:1::/48 is local-use NAT64 (RFC 8215): private whatever it embeds.
            matches!(s, [0x64, 0xff9b, 1, ..])
                || s[0] == 0x2002
                || (s[0] & 0xffc0) == 0xfec0
                || a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || a.is_unique_local()
                || a.is_unicast_link_local()
        }
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

fn is_public(ip: IpAddr) -> bool {
    !is_private(ip)
}

fn any(_: IpAddr) -> bool {
    true
}

/// Typed resolver error, found again in reqwest's error chain.
#[derive(Debug, thiserror::Error)]
#[error("address of {0} is not allowed")]
struct Blocked(String);

/// DNS resolver that refuses a name if any of its addresses is not allowed.
struct Guard(fn(IpAddr) -> bool);

impl Resolve for Guard {
    fn resolve(&self, name: Name) -> Resolving {
        let allowed = self.0;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() || addrs.iter().any(|a| !allowed(a.ip())) {
                return Err(Blocked(host).into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn client(guard: Guard) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(guard)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
}

impl Net {
    pub(crate) fn new(allow_private: bool) -> Result<Self, reqwest::Error> {
        // Err only means a provider is installed already, which is what we need.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            https: client(Guard(if allow_private { any } else { is_public }))?,
            http: allow_private
                .then(|| client(Guard(is_loopback)))
                .transpose()?,
            allow_private,
        })
    }

    /// The client for `url`, if its scheme, origin and IP literal pass.
    fn client_for(&self, url: &Url, origins: &[String]) -> Result<&reqwest::Client, NetError> {
        if !allowed(url, origins) {
            return Err(NetError::CrossOrigin(scrub(url.clone())));
        }
        let host = url.host_str().unwrap_or_default().to_owned();
        let (client, ok): (_, fn(IpAddr) -> bool) = match url.scheme() {
            "https" => (
                Some(&self.https),
                if self.allow_private { any } else { is_public },
            ),
            "http" => (self.http.as_ref(), is_loopback),
            _ => (None, any),
        };
        // IPv6 literals come bracketed (`[::1]`).
        let literal = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok();
        match client {
            Some(c) if literal.is_none_or(ok) => Ok(c),
            _ => Err(NetError::Blocked(host)),
        }
    }
}

impl<'a> Session<'a> {
    /// A session with `cx`'s Basic credentials.
    pub(crate) fn new(
        net: &'a Net,
        cx: &'a AccountCx,
        origins: &'a [String],
        mode: Mode,
    ) -> Result<Self, SyncError> {
        let Credential::Basic { username, password } = &cx.credential else {
            return Err(SyncError::Permanent("DAV needs Basic credentials".into()));
        };
        Ok(Self {
            net,
            cx,
            user: username,
            password,
            origins,
            mode,
        })
    }

    /// Sends one request, following redirects by the rules above.
    pub(crate) async fn send(
        &self,
        method: &str,
        url: &Url,
        depth: Option<&str>,
        body: String,
    ) -> Result<Reply, NetError> {
        let mut method = Method::from_bytes(method.as_bytes()).expect("valid method");
        let (mut url, mut depth, mut body) = (url.clone(), depth, body);
        for _ in 0..=MAX_REDIRECTS {
            let client = self.net.client_for(&url, self.origins)?;
            self.cx.limiter.acquire().await;
            let mut req = client
                .request(method.clone(), url.clone())
                .basic_auth(self.user, Some(self.password.expose_secret()));
            if !body.is_empty() {
                req = req
                    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                    .body(body.clone());
            }
            if let Some(d) = depth {
                req = req.header("Depth", d);
            }
            let mut resp = req.send().await.map_err(classify)?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let next = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .and_then(|l| url.join(l).ok());
                // A redirect without a usable Location is just an answer.
                if let Some(next) = next {
                    if !redirect_allowed(&url, &next, self.origins) {
                        return Err(NetError::CrossOrigin(scrub(next)));
                    }
                    if self.mode == Mode::Run && matches!(status, 301 | 308) {
                        return Err(NetError::Moved);
                    }
                    tracing::debug!(%url, %next, status, "DAV redirect");
                    url = next;
                    if status == 303 {
                        // See Other: fetch the answer, do not repeat the request.
                        (method, depth, body) = (Method::GET, None, String::new());
                    }
                    continue;
                }
            }
            let headers = resp.headers().clone();
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(classify)? {
                if body.len() + chunk.len() > MAX_BODY {
                    return Err(NetError::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(Reply {
                status,
                headers,
                body,
                url,
            });
        }
        Err(NetError::TooManyRedirects)
    }
}

/// A resolver refusal is [`NetError::Blocked`]; everything else is network.
fn classify(e: reqwest::Error) -> NetError {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(s) = src {
        if let Some(b) = s.downcast_ref::<Blocked>() {
            return NetError::Blocked(b.0.clone());
        }
        src = s.source();
    }
    NetError::Http(e)
}

impl Reply {
    /// `Retry-After` in seconds or as an HTTP date.
    pub(crate) fn retry_after(&self, now: jiff::Timestamp) -> Option<Duration> {
        let v = self.headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
        if let Ok(secs) = v.parse::<u64>() {
            return Some(Duration::from_secs(secs));
        }
        let at = jiff::fmt::rfc2822::DateTimeParser::new()
            .parse_timestamp(v)
            .ok()?;
        // A date in the past means "now".
        Some(Duration::try_from(at.duration_since(now)).unwrap_or(Duration::ZERO))
    }

    /// The run-time error for a status that is not the expected answer
    /// (plan, chapter 5.3).
    pub(crate) fn error(&self, now: jiff::Timestamp) -> SyncError {
        let what = || format!("DAV {} answered {}", self.url, self.status).into();
        match self.status {
            401 => SyncError::ReauthRequired {
                reason: musubi_model::ReauthReason::Rejected,
            },
            404 => SyncError::Rediscover,
            429 => SyncError::RateLimited {
                retry_after: self.retry_after(now),
            },
            503 if self.headers.contains_key(RETRY_AFTER) => SyncError::RateLimited {
                retry_after: self.retry_after(now),
            },
            500..=599 => SyncError::Transient(what()),
            _ => SyncError::Permanent(what()),
        }
    }
}

impl NetError {
    /// The run-time error (plan, chapter 5.3).
    pub(crate) fn into_sync(self) -> SyncError {
        match self {
            NetError::CrossOrigin(_) => SyncError::ReauthRequired {
                reason: musubi_model::ReauthReason::EndpointMoved,
            },
            NetError::Moved => SyncError::Rediscover,
            NetError::Http(e) => SyncError::Transient(e.into()),
            e => SyncError::Permanent(format!("{e:?}").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "::10.0.0.1",
            "64:ff9b::127.0.0.1",
            "64:ff9b::a00:1",
            "::ffff:0:7f00:1",
            "::ffff:0:a00:1",
            "64:ff9b:1::808:808",
            "64:ff9b:1:ffff::1",
            "2002:808:808::1",
            "fec0::1",
            "feff::1",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "100.128.0.1",
            "192.0.2.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
            "::8.8.8.8",
            "64:ff9b::8.8.8.8",
            "::ffff:0:808:808",
            "64:ff9b:2::1",
            "2003::1",
        ] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        let now: jiff::Timestamp = "2026-11-13T12:00:00Z".parse().unwrap();
        let reply = |v: &str| Reply {
            status: 503,
            headers: HeaderMap::from_iter([(RETRY_AFTER, v.parse().unwrap())]),
            body: Vec::new(),
            url: Url::parse("https://h/").unwrap(),
        };
        let after = |v: &str| reply(v).retry_after(now);
        assert_eq!(after("120"), Some(Duration::from_secs(120)));
        assert_eq!(
            after("Fri, 13 Nov 2026 12:01:30 GMT"),
            Some(Duration::from_secs(90))
        );
        assert_eq!(after("Thu, 01 Jan 2026 00:00:00 GMT"), Some(Duration::ZERO));
        assert_eq!(after("soon"), None);
    }

    #[test]
    fn redirects_stay_inside_and_never_downgrade() {
        let u = |s: &str| Url::parse(s).unwrap();
        let origins = vec!["https://h".to_string(), "http://h".to_string()];
        let ok = |from: &str, to: &str| redirect_allowed(&u(from), &u(to), &origins);
        assert!(ok("https://h/a", "https://h/b"));
        assert!(ok("http://h/a", "https://h/b"));
        assert!(!ok("https://h/a", "http://h/b"), "https to http");
        assert!(!ok("https://h/a", "https://h:8443/b"), "port change");
        assert!(!ok("https://h/a", "https://u:p@h/b"), "userinfo");
        assert!(!ok("https://h/a", "https://evil/b"));
    }

    #[test]
    fn origins_are_normalised() {
        let o = |u: &str| origin(&Url::parse(u).unwrap());
        assert_eq!(
            o("https://P52-CalDAV.example.com:443/x/"),
            "https://p52-caldav.example.com"
        );
        assert_eq!(o("http://Host:80/"), "http://host");
        assert_eq!(o("https://host:8443/"), "https://host:8443");
    }

    #[test]
    fn targets_are_checked_before_sending() {
        let strict = Net::new(false).unwrap();
        let lax = Net::new(true).unwrap();
        let all: Vec<String> = [
            "https://10.0.0.1",
            "https://127.0.0.1:8443",
            "https://[::1]",
            "http://127.0.0.1:8080",
            "http://192.0.2.1",
            "https://192.0.2.1",
            "http://localhost:8080",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let check = |net: &Net, u: &str| net.client_for(&Url::parse(u).unwrap(), &all).map(|_| ());
        let blocked = |r: Result<(), NetError>| matches!(r, Err(NetError::Blocked(_)));
        // Private literals, and any http, without allow_private.
        for u in [
            "https://10.0.0.1/",
            "https://127.0.0.1:8443/",
            "https://[::1]/",
            "http://127.0.0.1:8080/",
            "http://localhost:8080/",
        ] {
            assert!(blocked(check(&strict, u)), "{u}");
        }
        assert!(check(&strict, "https://192.0.2.1/").is_ok());
        // allow_private: private https and loopback http, never http elsewhere.
        assert!(check(&lax, "https://10.0.0.1/").is_ok());
        assert!(check(&lax, "http://127.0.0.1:8080/").is_ok());
        assert!(check(&lax, "http://localhost:8080/").is_ok());
        assert!(blocked(check(&lax, "http://192.0.2.1/")));
        // Userinfo would add a second Authorization: refused, and not echoed.
        match check(&lax, "https://u:secret@192.0.2.1/") {
            Err(NetError::CrossOrigin(u)) => assert!(!u.as_str().contains("secret")),
            _ => panic!("userinfo accepted"),
        }
        // Outside the allowed origins: nothing is sent, whatever the address.
        assert!(matches!(
            check(&lax, "https://evil.example/"),
            Err(NetError::CrossOrigin(_))
        ));
    }
}
