//! Whole-binary harness: `musubi-syncd` as a child process over [`FakeGoogle`]
//! (the crash tests of `musubi-syncd/tests/crash.rs` and the `bench` binary).
//!
//! [`Daemon`] collects the child's stderr, waits for lines in it (the
//! `listening on` banner, `musubi-crashpoint:` markers) and sends SIGKILL on
//! demand and on drop. [`Api`] is the HTTP client side: tenants, keys, the
//! OAuth sign-in against the fake, `/v1/changes`. [`Rng`] makes every random
//! choice reproducible from a printed seed.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::google::FakeGoogle;

/// Instance admin key of [`config`].
pub const ADMIN_KEY: &str = "instance-admin-key-for-tests-0123456789";

/// Prefix of the line a crash point prints before it blocks.
pub const CRASH_MARK: &str = "musubi-crashpoint: ";

/// A `musubi-syncd` configuration listening on `listen` over `data`, signing
/// in against `g`.
pub fn config(listen: SocketAddr, data: &Path, g: &FakeGoogle) -> Value {
    json!({
        "listen": listen.to_string(),
        "data_dir": data,
        "public_url": format!("http://{listen}/"),
        "admin_key": ADMIN_KEY,
        "signing_key": "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
        "token_key": "AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=",
        "google": {
            "client_id": "musubi-test.apps.example",
            "client_secret": "test-secret",
            "auth_url": g.auth_url(),
            "token_url": g.token_url(),
            "api_base": g.calendar_api(),
        },
    })
}

/// A free loopback port. The node rebinds it on every restart: the fake's
/// channels deliver to the address in the configuration.
pub fn free_addr() -> SocketAddr {
    // shortcut: another process may take the port before the node binds it; fine on a test runner.
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr")
}

#[derive(Default)]
struct Log {
    lines: Vec<String>,
    closed: bool,
}

/// A child process whose stderr is collected; killed (SIGKILL) on drop.
pub struct Daemon {
    child: Child,
    log: Arc<(Mutex<Log>, Condvar)>,
}

impl Daemon {
    /// Starts `cmd` with stdin and stdout closed and stderr collected.
    pub fn spawn(mut cmd: Command) -> std::io::Result<Self> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let log = Arc::new((Mutex::new(Log::default()), Condvar::new()));
        let stderr = child.stderr.take().expect("piped");
        let sink = log.clone();
        std::thread::spawn(move || {
            let lines = BufReader::new(stderr).lines();
            for line in lines.map_while(Result::ok) {
                sink.0.lock().expect("log").lines.push(line);
                sink.1.notify_all();
            }
            sink.0.lock().expect("log").closed = true;
            sink.1.notify_all();
        });
        Ok(Self { child, log })
    }

    /// Starts `musubi-syncd serve` and waits until it listens.
    pub fn serve(bin: &Path, config: &Path, env: &[(&str, String)]) -> Self {
        let mut cmd = Command::new(bin);
        cmd.arg("serve").arg("--config").arg(config);
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        let d = Self::spawn(cmd).expect("spawn musubi-syncd");
        assert!(
            d.wait_line("musubi-syncd: listening on", Duration::from_secs(30))
                .is_some(),
            "musubi-syncd did not start:\n{}",
            d.tail(40)
        );
        d
    }

    /// Waits for a stderr line containing `needle` (from the start of the
    /// log); `None` on timeout or when the process closed stderr.
    pub fn wait_line(&self, needle: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        let (m, cv) = &*self.log;
        let mut log = m.lock().expect("log");
        loop {
            if let Some(l) = log.lines.iter().find(|l| l.contains(needle)) {
                return Some(l.clone());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            if log.closed {
                return None;
            }
            log = cv.wait_timeout(log, left).expect("log").0;
        }
    }

    /// The last `n` stderr lines.
    pub fn tail(&self, n: usize) -> String {
        let log = self.log.0.lock().expect("log");
        log.lines[log.lines.len().saturating_sub(n)..].join("\n")
    }

    /// Process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Resident set size in KiB (Linux `/proc`).
    pub fn rss_kib(&self) -> Option<u64> {
        let s = std::fs::read_to_string(format!("/proc/{}/status", self.pid())).ok()?;
        let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }

    /// Sends SIGKILL and reaps the process.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for the process to exit by itself.
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

/// SplitMix64: small, seedable, good enough to pick kill points and changes.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// HTTP client of one node.
#[derive(Clone)]
pub struct Api {
    /// `http://host:port`.
    pub base: String,
    /// The client (no redirects, so the fake's consent redirect can be read).
    pub http: reqwest::Client,
}

impl Api {
    /// A client of the node at `addr`.
    pub fn new(addr: SocketAddr) -> Self {
        // reqwest's `rustls-no-provider` needs a process default; another
        // caller may have installed it already.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            base: format!("http://{addr}"),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(60))
                .build()
                .expect("client"),
        }
    }

    /// Sends a request; the status and the JSON body (`null` if none).
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> reqwest::Result<(u16, Value)> {
        let url = if path.starts_with("http") {
            path.to_owned()
        } else {
            format!("{}{path}", self.base)
        };
        let mut r = self.http.request(method.parse().expect("method"), url);
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    async fn ok(&self, method: &str, path: &str, auth: Option<&str>, body: Option<Value>) -> Value {
        let (s, v) = self.call(method, path, auth, body).await.expect("request");
        assert!((200..300).contains(&s), "{method} {path}: {s} {v}");
        v
    }

    /// Creates a tenant; returns its id.
    pub async fn tenant(&self) -> String {
        let v = self
            .ok("POST", "/v1/admin/tenants", Some(ADMIN_KEY), None)
            .await;
        v["id"].as_str().expect("id").to_owned()
    }

    /// Creates an API key with `scope` (`read` or `admin`).
    pub async fn key(&self, tenant: &str, scope: &str) -> String {
        let path = format!("/v1/admin/tenants/{tenant}/keys");
        let v = self
            .ok(
                "POST",
                &path,
                Some(ADMIN_KEY),
                Some(json!({ "scope": scope })),
            )
            .await;
        v["key"].as_str().expect("key").to_owned()
    }

    /// Signs in to the fake Google with the tenant's admin key: consent,
    /// then the callback. Returns the account id.
    pub async fn connect(&self, admin: &str) -> String {
        let v = self
            .ok(
                "POST",
                "/v1/accounts",
                Some(admin),
                Some(json!({ "provider": "google" })),
            )
            .await;
        let consent = self
            .http
            .get(v["authorization_url"].as_str().expect("url"))
            .send()
            .await
            .expect("consent");
        let callback = consent.headers()["location"]
            .to_str()
            .expect("ascii")
            .to_owned();
        let v = self.ok("GET", &callback, None, None).await;
        v["account"].as_str().expect("account").to_owned()
    }

    /// `GET /v1/changes?since=…&limit=…`.
    pub async fn changes(
        &self,
        key: &str,
        since: u64,
        limit: u32,
    ) -> reqwest::Result<(u16, Value)> {
        let path = format!("/v1/changes?since={since}&limit={limit}");
        self.call("GET", &path, Some(key), None).await
    }
}
