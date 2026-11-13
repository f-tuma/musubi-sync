//! A node on a free port over a temporary directory, the fake Google server,
//! HTTP helpers and store fixtures (as in musubi-store-sqlite's tests).
#![allow(dead_code)]

pub mod whole;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use musubi_model::provider::{Access, Pass, RemoteCollection};
use musubi_model::remote::MediaType;
use musubi_model::store::{DiscoveryWrite, ItemWrite, OccurrenceRow, Occurrences, PageWrite};
use musubi_model::{
    AccountId, CollectionId, CollectionRef, EntityKind, Raw, RemoteKey, Seq, Storage, TenantId,
    TenantStore, Validator,
};
use musubi_syncd::App;
use musubi_syncd::config::Config;
use musubi_testkit::google::FakeGoogle;
use serde_json::value::RawValue;
use serde_json::{Value, json};

pub const ADMIN_KEY: &str = "instance-admin-key-for-tests-0123456789";

pub struct Node {
    pub app: Arc<App>,
    pub base: String,
    pub dir: tempfile::TempDir,
    pub google: FakeGoogle,
    pub http: reqwest::Client,
}

/// The configuration file contents for a node at `listen` over `data`.
pub fn config_json(listen: &str, data: &Path, g: &FakeGoogle) -> Value {
    json!({
        "listen": listen,
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
            "tasks_api_base": g.tasks_api(),
            "people_api_base": g.people_api(),
        },
    })
}

impl Node {
    pub async fn start() -> Self {
        let google = FakeGoogle::start(Default::default())
            .await
            .expect("fake google");
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let config: Config =
            serde_json::from_value(config_json(&addr, &dir.path().join("data"), &google))
                .expect("config");
        let app = App::open(&config).expect("open");
        let router = app.clone().router();
        tokio::spawn(async move { axum::serve(listener, router).await });
        Self {
            app,
            base: format!("http://{addr}"),
            dir,
            google,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client"),
        }
    }

    /// Sends a request; returns the status and the JSON body (`null` if none).
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut r = self
            .http
            .request(method.parse().expect("method"), self.url(path));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.expect("send");
        let status = resp.status().as_u16();
        if resp
            .headers()
            .get("content-type")
            .is_some_and(|c| c.as_bytes().starts_with(b"text/event-stream"))
        {
            return (status, Value::Null);
        }
        let text = resp.text().await.expect("body");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_owned()
        } else {
            format!("{}{path}", self.base)
        }
    }

    pub async fn get(&self, path: &str, auth: &str) -> (u16, Value) {
        self.call("GET", path, Some(auth), None).await
    }

    /// Creates a tenant over the admin API.
    pub async fn tenant(&self) -> TenantId {
        let (s, v) = self
            .call("POST", "/v1/admin/tenants", Some(ADMIN_KEY), None)
            .await;
        assert_eq!(s, 201, "{v}");
        v["id"].as_str().expect("id").parse().expect("tenant id")
    }

    /// Creates an API key; returns the key and its id.
    pub async fn key(&self, t: TenantId, scope: &str) -> (String, String) {
        let (s, v) = self
            .call(
                "POST",
                &format!("/v1/admin/tenants/{t}/keys"),
                Some(ADMIN_KEY),
                Some(json!({ "scope": scope })),
            )
            .await;
        assert_eq!(s, 201, "{v}");
        (
            v["key"].as_str().expect("key").into(),
            v["id"].as_str().expect("id").into(),
        )
    }

    /// Discovers collection `name` for `account` and writes `items` into it;
    /// returns the collection and the journal head after the commit.
    pub fn write(
        &self,
        t: TenantId,
        account: AccountId,
        name: &str,
        items: Vec<ItemWrite>,
    ) -> (CollectionId, Seq) {
        write(self.app.storage(), t, account, name, items)
    }
}

pub fn write<S: Storage>(
    storage: &S,
    t: TenantId,
    account: AccountId,
    name: &str,
    items: Vec<ItemWrite>,
) -> (CollectionId, Seq) {
    let mut s = storage.open(t).expect("open tenant");
    s.apply_discovery(&DiscoveryWrite {
        account,
        collections: vec![remote(name)],
        complete: false,
    })
    .expect("discovery");
    let col = s
        .collections()
        .expect("collections")
        .into_iter()
        .find(|c| c.remote.remote.0 == name)
        .expect("collection")
        .id;
    let head = s.apply_page(&page(col, items)).expect("page");
    (col, head)
}

/// `h` hours after 2026-01-05T00:00Z.
pub fn t(h: i64) -> Timestamp {
    "2026-01-05T00:00:00Z".parse::<Timestamp>().expect("ts") + SignedDuration::from_hours(h)
}

pub fn remote(name: &str) -> RemoteCollection {
    RemoteCollection::new(
        CollectionRef(name.into()),
        EntityKind::Event,
        name,
        Access::Owner,
    )
}

/// A one-hour event at hour `h`.
pub fn event(key: &str, version: &str, h: i64) -> ItemWrite {
    let body = format!(r#"{{"id":"{key}","v":"{version}"}}"#);
    let raw = Raw {
        media_type: MediaType::Json,
        body: body.clone().into_bytes(),
    };
    ItemWrite {
        key: RemoteKey(key.into()),
        validator: Validator(format!("{key}@{version}")),
        raw_hash: raw.hash(),
        raw,
        kind: EntityKind::Event,
        canonical: Some(RawValue::from_string(body).expect("json")),
        schema_version: 1,
        mapping_version: 1,
        correlation: None,
        occurrences: Occurrences {
            rows: vec![OccurrenceRow {
                start: t(h),
                end: t(h + 1),
                recurrence_id: None,
                floating: false,
            }],
            series: None,
        },
    }
}

pub fn page(col: CollectionId, upserts: Vec<ItemWrite>) -> PageWrite {
    PageWrite {
        collection: col,
        pass: Pass::Delta,
        read: None,
        last: false,
        cursor: None,
        upserts,
        unchanged: Vec::new(),
        removals: Vec::new(),
    }
}

/// Reads one SSE event's `data` as JSON, or `None` after `wait`.
pub async fn next_event(
    resp: &mut reqwest::Response,
    buf: &mut String,
    wait: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        while let Some(end) = buf.find("\n\n") {
            let ev: String = buf.drain(..end + 2).collect();
            if let Some(data) = ev.lines().find_map(|l| l.strip_prefix("data: ")) {
                return Some(serde_json::from_str(data).expect("event json"));
            }
        }
        let chunk = tokio::time::timeout_at(deadline, resp.chunk()).await.ok()?;
        buf.push_str(std::str::from_utf8(&chunk.expect("chunk")?).expect("utf8"));
    }
}
