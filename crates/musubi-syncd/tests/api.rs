//! The HTTP API against a node in this process (K6 acceptance, plan ch. 5).

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::{ADMIN_KEY, Node, event, next_event};
use jiff::{SignedDuration, Timestamp};
use musubi_model::capability::ProviderId;
use musubi_model::control::{
    AccountRecord, AccountState, AccountTokens, ChannelRecord, ChannelState, Credentials, JobKind,
    Lane, Scope,
};
use musubi_model::provider::{ChannelId, Subscription};
use musubi_model::{
    AccountId, CollectionRef, ControlStore, Storage, StoreError, TenantId, TenantStore, TokenVault,
};
use musubi_syncd::crypto::Claims;
use secrecy::ExposeSecret;
use serde_json::{Value, json};

fn account(n: &Node, tenant: TenantId, id: u128) -> AccountId {
    let id = AccountId::from_u128(id);
    n.app
        .control()
        .upsert_account(&AccountRecord {
            id,
            tenant,
            provider: ProviderId("google".into()),
            subject: format!("subject-{id}"),
            state: AccountState::Active,
        })
        .expect("account");
    id
}

fn token(n: &Node, tenant: TenantId, scope: Scope, from: i64, to: i64) -> String {
    let now = Timestamp::now().as_second();
    n.app.issue_token(&Claims {
        tenant,
        scope,
        iat: now + from,
        exp: now + to,
    })
}

/// An error body carries nothing but the error.
fn no_data(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.keys().all(|k| k == "error" || k == "message"))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Need {
    /// A tenant credential with read scope.
    Read,
    /// A tenant credential with admin scope.
    Admin,
    /// A tenant API key (not a token).
    Key,
    /// The instance admin key.
    Instance,
}

struct Route {
    method: &'static str,
    template: &'static str,
    path: String,
    body: Option<Value>,
    need: Need,
}

/// `(METHOD, path)` of every operation in openapi.yaml.
fn openapi_operations() -> BTreeSet<(String, String)> {
    let spec = include_str!("../openapi.yaml");
    let mut out = BTreeSet::new();
    let mut path: Option<&str> = None;
    for line in spec.lines() {
        if let Some(p) = line.strip_prefix("  /").and_then(|p| p.strip_suffix(':')) {
            path = Some(&line[2..2 + p.len() + 1]);
        } else if line.len() - line.trim_start().len() < 4 {
            path = None;
        } else if let (Some(p), Some(m)) = (
            path,
            line.strip_prefix("    ")
                .and_then(|m| m.strip_suffix(':'))
                .filter(|m| ["get", "post", "put", "patch", "delete"].contains(m)),
        ) {
            out.insert((m.to_uppercase(), p.to_owned()));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn every_route_checks_credential_scope_and_tenant() {
    let n = Node::start().await;
    let (a, b, spare) = (n.tenant().await, n.tenant().await, n.tenant().await);
    let (a_admin, _) = n.key(a, "admin").await;
    let (a_read, _) = n.key(a, "read").await;
    let (b_admin, _) = n.key(b, "admin").await;
    let (revoked, revoked_id) = n.key(a, "admin").await;
    let (_, spare_key_id) = n.key(a, "read").await;
    let s = n
        .call(
            "DELETE",
            &format!("/v1/admin/keys/{revoked_id}"),
            Some(ADMIN_KEY),
            None,
        )
        .await;
    assert_eq!(s.0, 204);

    let acc = account(&n, a, 77);
    n.write(a, acc, "cal", vec![event("e1", "1", 1)]);
    let window = "from=2026-01-05T00:00:00Z&to=2026-01-06T00:00:00Z";
    let (_, v) = n.get(&format!("/v1/entities?{window}"), &a_read).await;
    let entity = v["occurrences"][0]["entity"]
        .as_str()
        .expect("entity")
        .to_owned();
    let (_, v) = n
        .get(
            &format!("/v1/entities/{entity}?include=placements"),
            &a_read,
        )
        .await;
    let item = v["placements"][0]["item"]
        .as_str()
        .expect("item")
        .to_owned();

    let a_token = token(&n, a, Scope::Admin, 0, 600);
    let a_read_token = token(&n, a, Scope::Read, 0, 600);
    let expired = token(&n, a, Scope::Admin, -600, -1);
    let mut tampered = a_token.clone().into_bytes();
    let i = tampered.len() - 10;
    tampered[i] = if tampered[i] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).expect("ascii");
    let backup_to = n.dir.path().join("matrix-backup");

    let r = |method, template, path: String, body: Option<Value>, need| Route {
        method,
        template,
        path,
        body,
        need,
    };
    let routes = vec![
        r(
            "POST",
            "/v1/accounts",
            "/v1/accounts".into(),
            Some(json!({ "provider": "google" })),
            Need::Admin,
        ),
        r(
            "GET",
            "/v1/accounts/{id}",
            format!("/v1/accounts/{acc}"),
            None,
            Need::Read,
        ),
        r(
            "GET",
            "/v1/collections",
            "/v1/collections".into(),
            None,
            Need::Read,
        ),
        r(
            "GET",
            "/v1/entities",
            format!("/v1/entities?{window}"),
            None,
            Need::Read,
        ),
        r(
            "GET",
            "/v1/entities/{id}",
            format!("/v1/entities/{entity}?include=placements"),
            None,
            Need::Read,
        ),
        r(
            "GET",
            "/v1/items/{id}",
            format!("/v1/items/{item}?include=raw"),
            None,
            Need::Read,
        ),
        r(
            "GET",
            "/v1/changes",
            "/v1/changes?since=0".into(),
            None,
            Need::Read,
        ),
        r("GET", "/v1/stream", "/v1/stream".into(), None, Need::Read),
        r(
            "POST",
            "/v1/tokens",
            "/v1/tokens".into(),
            Some(json!({})),
            Need::Key,
        ),
        r(
            "POST",
            "/v1/admin/tenants",
            "/v1/admin/tenants".into(),
            None,
            Need::Instance,
        ),
        r(
            "DELETE",
            "/v1/admin/tenants/{id}",
            format!("/v1/admin/tenants/{spare}"),
            None,
            Need::Instance,
        ),
        r(
            "POST",
            "/v1/admin/tenants/{id}/keys",
            format!("/v1/admin/tenants/{a}/keys"),
            Some(json!({ "scope": "read" })),
            Need::Instance,
        ),
        r(
            "GET",
            "/v1/admin/tenants/{id}/stats",
            format!("/v1/admin/tenants/{a}/stats"),
            None,
            Need::Instance,
        ),
        r(
            "DELETE",
            "/v1/admin/keys/{id}",
            format!("/v1/admin/keys/{spare_key_id}"),
            None,
            Need::Instance,
        ),
        r(
            "POST",
            "/v1/admin/backup",
            "/v1/admin/backup".into(),
            Some(json!({ "to": backup_to })),
            Need::Instance,
        ),
    ];
    // Authenticated by the OAuth state and the channel token; tested below.
    let public = [
        ("GET", "/v1/oauth/google/callback"),
        ("POST", "/v1/webhooks/google"),
    ];

    let mut covered: BTreeSet<(String, String)> = routes
        .iter()
        .map(|r| (r.method.to_owned(), r.template.to_owned()))
        .collect();
    covered.extend(
        public
            .iter()
            .map(|(m, p)| ((*m).to_owned(), (*p).to_owned())),
    );
    assert_eq!(
        covered,
        openapi_operations(),
        "the matrix covers exactly the OpenAPI operations"
    );

    for rt in &routes {
        let call =
            async |auth: Option<&str>| n.call(rt.method, &rt.path, auth, rt.body.clone()).await;
        let what = format!("{} {}", rt.method, rt.path);
        for bad in [
            None,
            Some("garbage"),
            Some(expired.as_str()),
            Some(&tampered),
            Some(&revoked),
        ] {
            let (s, v) = call(bad).await;
            assert_eq!(s, 401, "{what} with {bad:?}: {v}");
            assert!(no_data(&v), "{what}: {v}");
        }
        let forbidden: Vec<&str> = match rt.need {
            Need::Read => vec![ADMIN_KEY],
            Need::Admin => vec![ADMIN_KEY, &a_read, &a_read_token],
            Need::Key => vec![ADMIN_KEY, &a_token],
            Need::Instance => vec![&a_admin, &a_token],
        };
        for cred in forbidden {
            let (s, v) = call(Some(cred)).await;
            assert_eq!(s, 403, "{what} with {cred}: {v}");
            assert!(no_data(&v), "{what}: {v}");
        }
        // Another tenant's credential sees none of tenant A's data.
        if rt.need == Need::Read {
            let (s, v) = call(Some(&b_admin)).await;
            match rt.template {
                "/v1/accounts/{id}" | "/v1/entities/{id}" | "/v1/items/{id}" => {
                    assert_eq!(s, 404, "{what}: {v}");
                    assert!(no_data(&v), "{what}: {v}");
                }
                "/v1/collections" => assert_eq!(v["collections"], json!([]), "{what}"),
                "/v1/entities" => assert_eq!(v["occurrences"], json!([]), "{what}"),
                "/v1/changes" => assert_eq!(v["changes"], json!([]), "{what}"),
                "/v1/stream" => assert_eq!(s, 200, "B's own stream; content tested below"),
                t => panic!("no foreign-tenant check for {t}"),
            }
        }
        let ok = match rt.need {
            Need::Read => vec![a_read.as_str(), &a_read_token, &a_admin],
            Need::Admin => vec![a_admin.as_str(), &a_token],
            Need::Key => vec![a_read.as_str(), &a_admin],
            Need::Instance => vec![ADMIN_KEY],
        };
        for cred in ok {
            let (s, v) = call(Some(cred)).await;
            assert!((200..300).contains(&s), "{what} with {cred}: {s} {v}");
            if rt.need == Need::Instance {
                break; // deletes and backups succeed once
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_carry_at_most_the_key_scope_and_need_no_database() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let (read, read_id) = n.key(a, "read").await;
    let (admin, _) = n.key(a, "admin").await;

    let (s, v) = n
        .call(
            "POST",
            "/v1/tokens",
            Some(&read),
            Some(json!({ "scope": "admin" })),
        )
        .await;
    assert_eq!(s, 403, "{v}");
    let (s, v) = n
        .call(
            "POST",
            "/v1/tokens",
            Some(&read),
            Some(json!({ "ttl": 7200 })),
        )
        .await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = n
        .call(
            "POST",
            "/v1/tokens",
            Some(&admin),
            Some(json!({ "scope": "read" })),
        )
        .await;
    assert_eq!((s, v["scope"].as_str()), (201, Some("read")), "{v}");
    let (s, v) = n
        .call(
            "POST",
            "/v1/tokens",
            Some(&read),
            Some(json!({ "ttl": 60 })),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    let tok = v["token"].as_str().expect("token").to_owned();
    assert!(tok.starts_with("mt1."));
    assert_eq!(n.get("/v1/collections", &tok).await.0, 200);

    // Verification is signature and clock only: revoking the key does not
    // reach an issued token (bounded by its lifetime), while the key stops.
    let s = n
        .call(
            "DELETE",
            &format!("/v1/admin/keys/{read_id}"),
            Some(ADMIN_KEY),
            None,
        )
        .await;
    assert_eq!(s.0, 204);
    assert_eq!(n.get("/v1/collections", &read).await.0, 401);
    assert_eq!(n.get("/v1/collections", &tok).await.0, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_keys_are_stored_only_as_hashes() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let (key, id) = n.key(a, "admin").await;
    assert_eq!(n.get("/v1/collections", &key).await.0, 200);
    let mut stored = Vec::new();
    for f in std::fs::read_dir(n.dir.path().join("data")).expect("data dir") {
        let p = f.expect("entry").path();
        if p.file_name()
            .is_some_and(|f| f.to_string_lossy().starts_with("control.db"))
        {
            stored.extend(std::fs::read(&p).expect("read"));
        }
    }
    let has = |needle: &[u8]| stored.windows(needle.len()).any(|w| w == needle);
    let secret = key.strip_prefix("mk_").expect("prefix");
    assert!(!has(secret.as_bytes()), "the key is not stored");
    let hash: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&id[2 * i..2 * i + 2], 16).expect("hex"))
        .collect();
    assert!(has(&hash), "its SHA-256 is");
}

#[tokio::test(flavor = "multi_thread")]
async fn sealed_account_tokens_do_not_open_in_another_row() {
    let n = Node::start().await;
    let (a, b) = (n.tenant().await, n.tenant().await);
    let (one, two, other) = (account(&n, a, 1), account(&n, a, 2), account(&n, b, 3));
    let vault = n.app.vault();
    let tokens = AccountTokens {
        refresh: "refresh-1".to_owned().into(),
        access: "access-1".to_owned().into(),
        access_until: Timestamp::now(),
    };
    vault
        .store(one, &Credentials::OAuth(tokens))
        .expect("store");
    let Some(Credentials::OAuth(back)) = vault.load(one).expect("load") else {
        panic!("OAuth tokens");
    };
    assert_eq!(back.refresh.expose_secret(), "refresh-1");

    let c = n.app.control();
    let sealed = c.read_tokens(one).expect("read").expect("sealed");
    assert!(
        !sealed.0.windows(9).any(|w| w == b"refresh-1"),
        "encrypted at rest"
    );
    for to in [two, other] {
        c.write_tokens(to, &sealed).expect("move the blob");
        assert!(
            matches!(vault.load(to), Err(StoreError::Corrupt(_))),
            "a blob moved to {to} must not open"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_rejects_unknown_and_forged_and_enqueues_a_valid_one_fast() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let acc = account(&n, a, 9);
    let (col, _) = n.write(a, acc, "cal", vec![]);
    let control = n.app.control();
    control
        .put_channel(&ChannelRecord {
            tenant: a,
            account: acc,
            collection: col,
            state: ChannelState::Open(Subscription::new(
                ChannelId("ch-1".into()),
                CollectionRef("cal".into()),
                "res-1",
                "channel-secret".to_owned().into(),
                Timestamp::now() + SignedDuration::from_hours(24),
            )),
        })
        .expect("channel");
    control
        .put_channel(&ChannelRecord {
            tenant: a,
            account: acc,
            collection: col,
            state: ChannelState::Pending(ChannelId("ch-pending".into())),
        })
        .expect("pending channel");
    let hook = async |ch: &str, token: &str, resource: &str, state: &str| {
        n.http
            .post(n.url("/v1/webhooks/google"))
            .header("x-goog-channel-id", ch)
            .header("x-goog-channel-token", token)
            .header("x-goog-resource-id", resource)
            .header("x-goog-resource-state", state)
            .header("x-goog-message-number", "2")
            .send()
            .await
            .expect("send")
            .status()
            .as_u16()
    };
    let jobs = || {
        control
            .lease_due(
                Lane::Sync,
                Timestamp::now() + SignedDuration::from_hours(1),
                100,
                Duration::from_secs(1),
            )
            .expect("lease")
    };

    assert_eq!(
        hook("ch-unknown", "channel-secret", "res-1", "exists").await,
        404
    );
    assert_eq!(hook("ch-1", "wrong-secret", "res-1", "exists").await, 401);
    for state in ["sync", "exists"] {
        assert_eq!(
            hook("ch-pending", "", "res-1", state).await,
            404,
            "a pending channel accepts nothing"
        );
    }
    assert_eq!(
        hook("ch-1", "channel-secret", "res-other", "exists").await,
        404
    );
    assert_eq!(
        hook("ch-1", "channel-secret", "res-1", "sync").await,
        200,
        "handshake"
    );
    assert!(
        jobs().is_empty(),
        "rejected notifications and the handshake enqueue nothing"
    );

    let started = Instant::now();
    assert_eq!(hook("ch-1", "channel-secret", "res-1", "exists").await, 200);
    let took = started.elapsed();
    assert!(took < Duration::from_millis(50), "answered in {took:?}");
    // A burst coalesces into the one pending job.
    assert_eq!(hook("ch-1", "channel-secret", "res-1", "exists").await, 200);
    let leased = jobs();
    assert_eq!(leased.len(), 1, "{leased:?}");
    assert_eq!(leased[0].spec.tenant, a);
    assert_eq!(
        leased[0].spec.kind,
        JobKind::Sync {
            account: acc,
            collection: col
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_and_stream_deliver_the_cursor_after_commit_to_its_tenant_only() {
    let n = Node::start().await;
    let (a, b) = (n.tenant().await, n.tenant().await);
    let (ka, _) = n.key(a, "read").await;
    let (kb, _) = n.key(b, "read").await;
    let acc = account(&n, a, 5);
    let (col, first) = n.write(a, acc, "cal", vec![event("e1", "1", 1)]);

    let open = async |key: &str, since: Option<u64>| {
        let path = match since {
            Some(s) => format!("/v1/stream?since={s}"),
            None => "/v1/stream".into(),
        };
        let r = n
            .http
            .get(n.url(&path))
            .bearer_auth(key)
            .send()
            .await
            .expect("stream");
        assert_eq!(r.status(), 200);
        r
    };
    let wait = Duration::from_secs(5);
    let (mut sa, mut bufa) = (open(&ka, Some(first.0)).await, String::new());
    let (mut sb, mut bufb) = (open(&kb, None).await, String::new());
    let head_b = next_event(&mut sb, &mut bufb, wait)
        .await
        .expect("b's head");
    assert_eq!(head_b["cursor"], 0);

    // Commit, then announce: what the engine's writer does.
    let head = n
        .app
        .storage()
        .open(a)
        .expect("open")
        .apply_page(&common::page(col, vec![event("e2", "1", 2)]))
        .expect("page");
    assert!(head > first);
    n.app.bus().notify(a, head);
    let ev = next_event(&mut sa, &mut bufa, wait)
        .await
        .expect("a's event");
    assert_eq!(ev["cursor"], head.0);

    let (s, v) = n.get(&format!("/v1/changes?since={}", first.0), &ka).await;
    assert_eq!(s, 200, "{v}");
    assert!(!v["changes"].as_array().expect("changes").is_empty(), "{v}");
    assert_eq!(v["next"], head.0, "{v}");

    assert_eq!(
        next_event(&mut sb, &mut bufb, Duration::from_millis(300)).await,
        None,
        "another tenant's stream gets nothing"
    );
    // A reconnect with an older cursor gets the head at once.
    let mut again = open(&ka, Some(first.0)).await;
    let ev = next_event(&mut again, &mut String::new(), wait)
        .await
        .expect("head");
    assert_eq!(ev["cursor"], head.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pruned_cursor_needs_a_new_initial_sync() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let (key, _) = n.key(a, "read").await;
    let acc = account(&n, a, 5);
    n.write(a, acc, "cal", vec![event("e1", "1", 1)]);
    assert_eq!(n.get("/v1/changes?since=0", &key).await.0, 200);

    let mut t = n.app.storage().open(a).expect("open");
    t.prune(Timestamp::now() + SignedDuration::from_hours(1))
        .expect("prune");
    t.apply_page(&common::page(
        t.collections().expect("cols")[0].id,
        vec![event("e2", "1", 2)],
    ))
    .expect("page");

    for path in ["/v1/changes?since=0", "/v1/stream?since=0"] {
        let (s, v) = n.get(path, &key).await;
        assert_eq!(s, 410, "{path}: {v}");
        assert_eq!(v["error"], "cursor_too_old", "{v}");
        assert!(
            v["message"]
                .as_str()
                .expect("message")
                .contains("new initial sync required"),
            "{v}"
        );
        assert!(v["oldest"].as_u64().is_some(), "{v}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connecting_an_account_runs_pkce_once_and_seals_its_tokens() {
    let n = Node::start().await;
    let (a, b) = (n.tenant().await, n.tenant().await);
    let (admin, _) = n.key(a, "admin").await;
    let (b_admin, _) = n.key(b, "admin").await;

    let start = async |key: &str, body: Value| {
        let (s, v) = n.call("POST", "/v1/accounts", Some(key), Some(body)).await;
        assert_eq!(s, 201, "{v}");
        v["authorization_url"].as_str().expect("url").to_owned()
    };
    // The browser: consent page, then the redirect back to the node.
    let consent = async |url: &str| {
        let r = n.http.get(url).send().await.expect("consent");
        assert_eq!(r.status(), 302);
        r.headers()["location"].to_str().expect("ascii").to_owned()
    };

    let url = start(&admin, json!({ "provider": "google" })).await;
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    let callback = consent(&url).await;
    assert!(callback.starts_with(&n.base), "{callback}");
    let (s, v) = n.call("GET", &callback, None, None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "connecting");
    let id: AccountId = v["account"].as_str().expect("account").parse().expect("id");

    let (s, v) = n.call("GET", &callback, None, None).await;
    assert_eq!(s, 400, "a state works once: {v}");

    let rec = n.app.control().account(id).expect("read").expect("account");
    assert_eq!(
        (rec.tenant, rec.subject.as_str()),
        (a, "100000000000000000001")
    );
    let Some(Credentials::OAuth(tokens)) = n.app.vault().load(id).expect("load") else {
        panic!("OAuth tokens");
    };
    assert_eq!(tokens.refresh.expose_secret(), n.google.refresh_token());

    // Signing in as the same user again keeps the account.
    let callback = consent(&start(&admin, json!({ "provider": "google" })).await).await;
    let (s, v) = n.call("GET", &callback, None, None).await;
    assert_eq!(
        (s, v["account"].as_str()),
        (200, Some(id.to_string().as_str())),
        "{v}"
    );
    // Reconnecting another tenant's account is not possible.
    let (s, v) = n
        .call(
            "POST",
            "/v1/accounts",
            Some(&b_admin),
            Some(json!({ "provider": "google", "account": id })),
        )
        .await;
    assert_eq!(s, 404, "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejections_and_unknown_routes_answer_the_error_json() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let (key, _) = n.key(a, "admin").await;
    let bad_json = n
        .http
        .post(n.url("/v1/tokens"))
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("send");
    let s = bad_json.status().as_u16();
    let v: Value = bad_json.json().await.expect("json body");
    assert_eq!(s, 400, "{v}");
    assert!(no_data(&v) && v["message"].is_string(), "{v}");
    for (method, path, want) in [
        ("GET", "/v1/changes?since=abc", 400),
        ("GET", "/v1/items/not-an-id", 400),
        ("GET", "/v1/nothing-here", 404),
        ("PUT", "/v1/tokens", 405),
    ] {
        let (s, v) = n.call(method, path, Some(&key), None).await;
        assert_eq!(s, want, "{method} {path}: {v}");
        assert!(
            no_data(&v) && v["error"].is_string() && v["message"].is_string(),
            "{method} {path}: {v}"
        );
    }
    // The scheme is case-insensitive (RFC 9110).
    let r = n
        .http
        .get(n.url("/v1/collections"))
        .header("authorization", format!("bearer {key}"))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_ahead_of_the_journal_is_refused() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let (key, _) = n.key(a, "read").await;
    let acc = account(&n, a, 5);
    let (_, head) = n.write(a, acc, "cal", vec![event("e1", "1", 1)]);
    for path in ["/v1/changes", "/v1/stream"] {
        let (s, v) = n.get(&format!("{path}?since={}", head.0 + 1), &key).await;
        assert_eq!(s, 410, "{path}: {v}");
        assert_eq!(v["error"], "cursor_ahead", "{v}");
        assert_eq!(v["oldest"], head.0, "{v}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_are_capped_per_tenant_and_end_with_their_token() {
    let n = Node::start().await;
    let (a, b) = (n.tenant().await, n.tenant().await);
    let (ka, _) = n.key(a, "read").await;
    let (kb, _) = n.key(b, "read").await;
    let open = async |auth: &str| {
        n.http
            .get(n.url("/v1/stream"))
            .bearer_auth(auth)
            .send()
            .await
            .expect("stream")
    };
    let mut held = Vec::new();
    for _ in 0..16 {
        let r = open(&ka).await;
        assert_eq!(r.status(), 200);
        held.push(r);
    }
    let r = open(&ka).await;
    assert_eq!(r.status(), 429);
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["error"], "too_many_requests", "{v}");
    assert_eq!(open(&kb).await.status(), 200, "the cap is per tenant");
    drop(held.pop());
    // The slot frees once the server sees the disconnect.
    let mut ok = false;
    for _ in 0..100 {
        if open(&ka).await.status() == 200 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "a closed stream frees its slot");

    let tok = token(&n, b, Scope::Read, 0, 2);
    let mut r = open(&tok).await;
    assert_eq!(r.status(), 200);
    let started = Instant::now();
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while r.chunk().await.expect("chunk").is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "the stream ends when its token expires");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_that_outlives_its_tenant_cannot_start_a_connection() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let tok = token(&n, a, Scope::Admin, 0, 600);
    let (s, _) = n
        .call(
            "DELETE",
            &format!("/v1/admin/tenants/{a}"),
            Some(ADMIN_KEY),
            None,
        )
        .await;
    assert_eq!(s, 204);
    let (s, v) = n
        .call(
            "POST",
            "/v1/accounts",
            Some(&tok),
            Some(json!({ "provider": "google" })),
        )
        .await;
    assert_eq!(s, 404, "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_resumed_one_before_the_head_gets_the_head_and_what_follows() {
    let n = Node::start().await;
    let a = n.tenant().await;
    let (key, _) = n.key(a, "read").await;
    let acc = account(&n, a, 5);
    let (col, _) = n.write(a, acc, "cal", vec![event("e1", "1", 1)]);
    let commit = |uid: &str, day| {
        let head = n
            .app
            .storage()
            .open(a)
            .expect("open")
            .apply_page(&common::page(col, vec![event(uid, "1", day)]))
            .expect("page");
        n.app.bus().notify(a, head);
        head
    };
    let open = async |last: u64| {
        let r = n
            .http
            .get(n.url("/v1/stream"))
            .bearer_auth(&key)
            .header("Last-Event-ID", last.to_string())
            .send()
            .await
            .expect("stream");
        assert_eq!(r.status(), 200);
        (r, String::new())
    };
    let wait = Duration::from_secs(5);
    let cursor = |ev: Option<Value>| ev.expect("event")["cursor"].as_u64().expect("cursor");

    let head = commit("e2", 2);
    let (mut s, mut buf) = open(head.0 - 1).await;
    assert_eq!(cursor(next_event(&mut s, &mut buf, wait).await), head.0);
    let later = commit("e3", 3);
    assert_eq!(cursor(next_event(&mut s, &mut buf, wait).await), later.0);

    // Commits while the client was away: it gets the head they reached, once.
    let away = [commit("e4", 4), commit("e5", 5)][1];
    let (mut s, mut buf) = open(head.0 - 1).await;
    assert_eq!(cursor(next_event(&mut s, &mut buf, wait).await), away.0);
    let last = commit("e6", 6);
    assert_eq!(cursor(next_event(&mut s, &mut buf, wait).await), last.0);
    assert_eq!(
        next_event(&mut s, &mut buf, Duration::from_millis(300)).await,
        None,
        "nothing twice"
    );
}
