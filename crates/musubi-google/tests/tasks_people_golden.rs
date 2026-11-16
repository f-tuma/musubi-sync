//! Golden fixtures for `TasksCodec` and `PeopleCodec` (plan K6).
//!
//! Recorded bodies from T10 (`fixtures/live/t10/`) are decoded and asserted
//! in full; the synthetic fixtures in `fixtures/{tasks,people}/` cover only
//! what T10 did not record (their README lists which).

use musubi_google::{PeopleCodec, TasksCodec};
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{Contact, Kind, Raw, Task};
use serde_json::{Value, json};

fn decode<K: Kind, C: Codec<K>>(dir: &str, name: &str) -> K::Canonical {
    let path = format!(
        "{}/tests/fixtures/{dir}/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let body = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    match C::decode(&Raw {
        media_type: MediaType::Json,
        body,
    }) {
        Decoded::Canonical(v) => v,
        Decoded::Unsupported { reason } => panic!("{name}: unsupported: {reason}"),
        _ => panic!("{name}: unexpected decode result"),
    }
}

macro_rules! golden {
    ($kind:ty, $codec:ty, $dir:literal: $($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            insta::assert_json_snapshot!(decode::<$kind, $codec>($dir, &stringify!($name)[$dir.len() + 1..]));
        }
    )*};
}

golden!(Task, TasksCodec, "tasks": tasks_due, tasks_subtask, tasks_unknown_field);
golden!(
    Contact,
    PeopleCodec,
    "people": people_full,
    people_email_only,
    people_birthday_without_year,
    people_unknown_field,
);

/// The object at `pointer` of a T10 recording, decoded by production code.
fn recorded<K: Kind, C: Codec<K>>(file: &str, pointer: &str) -> Value
where
    K::Canonical: serde::Serialize,
{
    let path = format!(
        "{}/tests/fixtures/live/t10/{file}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let body = serde_json::to_vec(v.pointer(pointer).expect(pointer)).unwrap();
    match C::decode(&Raw {
        media_type: MediaType::Json,
        body,
    }) {
        Decoded::Canonical(c) => serde_json::to_value(c).unwrap(),
        _ => panic!("{file}{pointer}: no canonical view"),
    }
}

fn task(file: &str, pointer: &str) -> Value {
    recorded::<Task, TasksCodec>(file, pointer)
}

fn person(file: &str, pointer: &str) -> Value {
    recorded::<Contact, PeopleCodec>(file, pointer)
}

#[test]
fn recorded_open_task() {
    assert_eq!(
        task("tasks.insert", ""),
        json!({
            "uid": "OTExSGpJWHRkVWt0MnNBQQ", "title": "t10-d", "description": "musubi t10",
            "showWithoutTime": true, "progress": "needs-action", "hidden": false,
            "priority": 0, "updated": "2026-11-16T10:43:19.549Z", "sequence": 0,
        })
    );
}

#[test]
fn recorded_completed_task() {
    assert_eq!(
        task("tasks.patch.completed", ""),
        json!({
            "uid": "NURkVnJQZDhKUWlvWDFVNQ", "title": "t10-c", "description": "musubi t10",
            "showWithoutTime": true, "progress": "completed",
            "progressUpdated": "2026-11-16T10:43:27.004Z", "hidden": false,
            "priority": 0, "updated": "2026-11-16T10:43:27.025Z", "sequence": 0,
        })
    );
}

/// "Clear completed" hides the task (T10).
#[test]
fn recorded_hidden_task() {
    assert_eq!(
        task("tasks.list.deleted_hidden", "/items/0"),
        json!({
            "uid": "NURkVnJQZDhKUWlvWDFVNQ", "title": "t10-c", "description": "musubi t10",
            "showWithoutTime": true, "progress": "completed",
            "progressUpdated": "2026-11-16T10:43:27.004Z", "hidden": true,
            "priority": 0, "updated": "2026-11-16T10:43:27.326Z", "sequence": 0,
        })
    );
}

/// A deleted task keeps `status: needsAction` (T10). It decodes, but the
/// transport turns it into a removal (`tasks::tests`), so this view is never
/// stored.
#[test]
fn recorded_deleted_task() {
    let rec = task("tasks.list.deleted_hidden", "/items/1");
    assert_eq!(rec["uid"], "UDdEX0tBMkJUVUJQS212aA");
    assert_eq!(rec["progress"], "needs-action");
}

#[test]
fn recorded_created_and_updated_person() {
    let want = |family: &str, updated: &str| {
        json!({
            "uid": "people/c1975428914001392424", "kind": "individual",
            "name": {
                "components": [
                    { "kind": "given", "value": "Musubi" },
                    { "kind": "surname", "value": family },
                ],
                "full": format!("Musubi {family}"),
            },
            "emails": [{ "address": "t10@example.com", "pref": 1 }],
            "updated": updated,
        })
    };
    let created = want("T10", "2026-11-16T10:43:32.55516Z");
    assert_eq!(person("people.createContact", ""), created);
    assert_eq!(person("people.delta.created", "/connections/0"), created);
    assert_eq!(
        person("people.delta.updated", "/connections/0"),
        want("T10 v1", "2026-11-16T10:43:34.89391Z")
    );
}

/// A tombstone has only `resourceName`, `etag` and `metadata` (T10). The
/// transport makes it a removal (`people::tests`); decoded, only the uid is
/// left.
#[test]
fn recorded_deleted_person() {
    assert_eq!(
        person("people.delta.deleted", "/connections/0"),
        json!({ "uid": "people/c1975428914001392424", "kind": "individual", "updated": "1970-01-01T00:00:00Z" })
    );
}
