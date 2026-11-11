//! Recorded Google responses (K8) against the fake's shapes.
//!
//! Each `fixtures/google/recorded/<endpoint>.json` holds one real, scrubbed
//! response body; see the README there. Until K8 records them the directory
//! is empty and this test checks nothing.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::{get, post_form};
use musubi_testkit::google::{Config, FakeGoogle, Seed};
use serde_json::{Value, json};

/// Paths (array elements written `[]`) that real responses have and the fake
/// knowingly lacks. Every entry has a line in FIDELITY.md.
const DOCUMENTED: &[&str] = &[
    "error.status",
    "error.errors[].location",
    "error.errors[].locationType",
];

/// Paths present in `real` and absent from `fake`, at any depth, minus
/// [`DOCUMENTED`]. Arrays compare their first elements. A key only the fake
/// has is a deviation for FIDELITY.md, not a failure.
fn missing(real: &Value, fake: &Value) -> BTreeSet<String> {
    fn walk(real: &Value, fake: &Value, path: &str, out: &mut BTreeSet<String>) {
        match (real, fake) {
            (Value::Object(r), Value::Object(f)) => {
                for (k, rv) in r {
                    let p = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    match f.get(k) {
                        Some(fv) => walk(rv, fv, &p, out),
                        None => {
                            out.insert(p);
                        }
                    }
                }
            }
            (Value::Array(r), Value::Array(f)) => {
                if let (Some(rv), Some(fv)) = (r.first(), f.first()) {
                    walk(rv, fv, &format!("{path}[]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    walk(real, fake, "", &mut out);
    out.retain(|p| !DOCUMENTED.contains(&p.as_str()));
    out
}

#[test]
fn comparison_finds_nested_keys_and_skips_documented_ones() {
    let real = json!({ "error": { "code": 410, "status": "GONE", "errors": [{ "reason": "x", "location": "a", "extra": 1 }] }, "top": 1 });
    let fake = json!({ "error": { "code": 410, "errors": [{ "reason": "x" }] } });
    assert_eq!(
        missing(&real, &fake),
        BTreeSet::from(["error.errors[].extra".to_owned(), "top".to_owned()])
    );
}

#[tokio::test]
async fn recorded_responses_have_no_keys_the_fake_lacks() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/google/recorded");
    let g = FakeGoogle::start(Config {
        seed: Seed {
            seed: 1,
            calendars: 1,
            events: 20,
        },
        ..Config::default()
    })
    .await
    .expect("start");
    let t = g.access_token();
    let api = g.calendar_api();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("fixtures dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let real: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("JSON");
        let name = path.file_stem().and_then(|s| s.to_str()).expect("name");
        let fake = match name {
            "calendarList" => get(&format!("{api}/users/me/calendarList"), &t).await.json,
            "events.list" => {
                get(&format!("{api}/calendars/primary/events"), &t)
                    .await
                    .json
            }
            "events.list.410" => {
                g.apply(&musubi_testkit::google::Change::ExpireSyncTokens {
                    calendar: "primary".into(),
                });
                get(&format!("{api}/calendars/primary/events?syncToken=00"), &t)
                    .await
                    .json
            }
            "token" => {
                post_form(
                    &g.token_url(),
                    &format!(
                        "grant_type=refresh_token&refresh_token={}",
                        g.refresh_token()
                    ),
                )
                .await
                .json
            }
            other => panic!("no fake request mapped for recorded fixture {other}.json"),
        };
        assert_eq!(missing(&real, &fake), BTreeSet::new(), "{name}.json");
        checked += 1;
    }
    eprintln!("compared {checked} recorded fixtures");
}
