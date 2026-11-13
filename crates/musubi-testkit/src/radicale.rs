//! Radicale as a local DAV server for the conformance suite (plan, chapter 8.3).
//!
//! [`Radicale::start`] runs `python -m radicale` from the venv named by
//! `MUSUBI_RADICALE` on a free loopback port, over a temporary directory
//! holding the htpasswd file (plain passwords) and the storage. It creates the
//! user, one calendar (`VEVENT` and `VTODO`) and one address book. Dropping it
//! kills the process and removes the directory.
//!
//! The venv (CI job `conformance` does the same):
//!
//! ```text
//! python3 -m venv /tmp/radicale-venv
//! /tmp/radicale-venv/bin/pip install --require-hashes --only-binary :all: \
//!     -r crates/musubi-testkit/radicale/requirements.txt
//! MUSUBI_RADICALE=/tmp/radicale-venv cargo test -p musubi-testkit --test conformance
//! ```

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::harness::{Daemon, free_addr};

/// A running Radicale with one user, one calendar and one address book.
pub struct Radicale {
    // Field order is drop order: the process dies before its directory goes.
    daemon: Daemon,
    dir: tempfile::TempDir,
    base: String,
    http: reqwest::Client,
}

/// Basic auth user [`Radicale::start`] creates.
pub const USER: &str = "alice";
/// Its initial password.
pub const PASSWORD: &str = "secret";

impl Radicale {
    /// Starts Radicale and creates the user, `/alice/calendar/` and
    /// `/alice/contacts/`.
    ///
    /// # Panics
    /// If `MUSUBI_RADICALE` is unset (with instructions) or Radicale does not
    /// come up within 30 s.
    pub async fn start() -> Self {
        let venv = std::env::var_os("MUSUBI_RADICALE").unwrap_or_else(|| {
            panic!(
                "MUSUBI_RADICALE is not set; it must name a venv with Radicale:\n  \
                 python3 -m venv /tmp/radicale-venv\n  \
                 /tmp/radicale-venv/bin/pip install --require-hashes --only-binary :all: \
                 -r crates/musubi-testkit/radicale/requirements.txt\n  \
                 MUSUBI_RADICALE=/tmp/radicale-venv cargo test -p musubi-testkit --test conformance"
            )
        });
        let python = Path::new(&venv).join("bin/python");
        let dir = tempfile::tempdir().expect("tempdir");
        let htpasswd = dir.path().join("htpasswd");
        std::fs::write(&htpasswd, format!("{USER}:{PASSWORD}\n")).expect("htpasswd");
        let addr = free_addr();
        let mut cmd = Command::new(&python);
        cmd.args(["-m", "radicale", "--config", ""])
            .arg("--server-hosts")
            .arg(addr.to_string())
            .args(["--auth-type", "htpasswd", "--auth-htpasswd-filename"])
            .arg(&htpasswd)
            .args([
                "--auth-htpasswd-encryption",
                "plain",
                "--storage-filesystem-folder",
            ])
            .arg(dir.path().join("collections"))
            // Hermetic: no user config, no bytecode written into the venv.
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env_remove("RADICALE_CONFIG");
        let daemon =
            Daemon::spawn(cmd).unwrap_or_else(|e| panic!("cannot run {}: {e}", python.display()));
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::net::TcpStream::connect(addr).is_err() {
            assert!(
                Instant::now() < deadline,
                "Radicale did not start:\n{}",
                daemon.tail(40)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // reqwest's `rustls-no-provider` needs a process default; another
        // caller may have installed it already.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let r = Self {
            daemon,
            dir,
            base: format!("http://{addr}"),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .expect("client"),
        };
        // The first authenticated request creates the principal collection.
        r.request("PROPFIND", &format!("/{USER}/"), Some("0"), "")
            .await;
        let cal = "<?xml version=\"1.0\"?><C:mkcalendar xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">\
             <D:set><D:prop><D:displayname>Calendar</D:displayname>\
             <C:supported-calendar-component-set><C:comp name=\"VEVENT\"/><C:comp name=\"VTODO\"/></C:supported-calendar-component-set>\
             </D:prop></D:set></C:mkcalendar>";
        r.expect("MKCALENDAR", &r.calendar(), cal, 201).await;
        let book = "<?xml version=\"1.0\"?><D:mkcol xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\">\
             <D:set><D:prop><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype>\
             <D:displayname>Contacts</D:displayname></D:prop></D:set></D:mkcol>";
        r.expect("MKCOL", &r.addressbook(), book, 201).await;
        r
    }

    /// `http://127.0.0.1:<port>/`: the server address a user enters.
    pub fn url(&self) -> String {
        format!("{}/", self.base)
    }

    /// The calendar's path.
    pub fn calendar(&self) -> String {
        format!("/{USER}/calendar/")
    }

    /// The address book's path.
    pub fn addressbook(&self) -> String {
        format!("/{USER}/contacts/")
    }

    /// The temporary directory (htpasswd and storage).
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Process id.
    pub fn pid(&self) -> u32 {
        self.daemon.pid()
    }

    /// Replaces the user's password in the htpasswd file (Radicale reads it
    /// on every request, so the old password gets `401` at once).
    pub fn set_password(&self, password: &str) {
        std::fs::write(
            self.dir.path().join("htpasswd"),
            format!("{USER}:{password}\n"),
        )
        .expect("htpasswd");
    }

    /// Creates or replaces an object (`PUT`); returns its path.
    pub async fn put(&self, collection: &str, name: &str, body: &str) -> String {
        let path = format!("{collection}{name}");
        let (s, _) = self.request("PUT", &path, None, body).await;
        assert!(s == 201 || s == 204, "PUT {path}: {s}");
        path
    }

    /// Deletes an object.
    pub async fn delete(&self, path: &str) {
        self.expect("DELETE", path, "", 200).await;
    }

    /// Creates a collection with an extended `MKCOL` body (RFC 5689); returns its path.
    pub async fn mkcol(&self, path: &str, body: &str) -> String {
        self.expect("MKCOL", path, body, 201).await;
        path.to_owned()
    }

    /// Sends one request with the current credentials; status and body.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        depth: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let password = std::fs::read_to_string(self.dir.path().join("htpasswd"))
            .ok()
            .and_then(|l| l.trim().split_once(':').map(|(_, p)| p.to_owned()))
            .unwrap_or_default();
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .basic_auth(USER, Some(password))
            .body(body.to_owned());
        if let Some(d) = depth {
            req = req.header("depth", d);
        }
        if body.starts_with("<?xml") {
            req = req.header("content-type", "application/xml; charset=utf-8");
        } else if body.starts_with("BEGIN:VCARD") {
            req = req.header("content-type", "text/vcard; charset=utf-8");
        } else if !body.is_empty() {
            req = req.header("content-type", "text/calendar; charset=utf-8");
        }
        let resp = req.send().await.expect("Radicale request");
        let status = resp.status().as_u16();
        (status, resp.text().await.unwrap_or_default())
    }

    async fn expect(&self, method: &str, path: &str, body: &str, want: u16) {
        let (s, text) = self.request(method, path, None, body).await;
        assert_eq!(s, want, "{method} {path}: {text}\n{}", self.daemon.tail(20));
    }
}
