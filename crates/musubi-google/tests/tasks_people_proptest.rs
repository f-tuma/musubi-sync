//! `TasksCodec` and `PeopleCodec` are total and deterministic (plan K6).

use musubi_google::{PeopleCodec, TasksCodec};
use musubi_model::provider::Codec;
use musubi_model::remote::MediaType;
use musubi_model::{Contact, Raw, Task};
use proptest::prelude::*;
use serde_json::Value;

/// Decodes with both codecs twice; the same bytes give the same result.
fn decode(body: Vec<u8>) {
    let raw = Raw {
        media_type: MediaType::Json,
        body,
    };
    assert_eq!(
        <TasksCodec as Codec<Task>>::decode(&raw),
        <TasksCodec as Codec<Task>>::decode(&raw)
    );
    assert_eq!(
        <PeopleCodec as Codec<Contact>>::decode(&raw),
        <PeopleCodec as Codec<Contact>>::decode(&raw)
    );
}

fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        ".{0,12}".prop_map(Value::from),
        // Strings that almost parse as Google timestamps and types.
        prop_oneof![
            Just("2026-11-20T00:00:00.000Z"),
            Just("2026-11-20T23:59:59+14:00"),
            Just("2026-11-20"),
            Just("-009999-01-01T00:00:00Z"),
            Just("completed"),
            Just("needsAction"),
            Just("people/c1"),
            Just("homeFax"),
            Just("work"),
            Just("CONTACT"),
        ]
        .prop_map(Value::from),
    ]
}

const KEYS: &[&str] = &[
    // Task
    "id",
    "title",
    "notes",
    "status",
    "due",
    "completed",
    "updated",
    "hidden",
    "parent",
    "deleted",
    // Person
    "resourceName",
    "metadata",
    "sources",
    "type",
    "updateTime",
    "primary",
    "names",
    "displayName",
    "givenName",
    "familyName",
    "middleName",
    "honorificPrefix",
    "honorificSuffix",
    "nicknames",
    "value",
    "organizations",
    "name",
    "department",
    "emailAddresses",
    "phoneNumbers",
    "addresses",
    "formattedValue",
    "streetAddress",
    "city",
    "postalCode",
    "country",
    "urls",
    "biographies",
    "birthdays",
    "date",
    "year",
    "month",
    "day",
    "photos",
    "url",
    "default",
];

/// Objects built from the field names the codecs read, at any depth.
fn shaped() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((prop::sample::select(KEYS), inner), 0..10).prop_map(|kv| {
                Value::Object(kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
            }),
        ]
    })
}

/// A golden fixture with some top-level fields replaced by arbitrary values.
fn mutated_fixture() -> impl Strategy<Value = Value> {
    let fixtures: Vec<Value> = ["tasks", "people"]
        .into_iter()
        .flat_map(|dir| {
            std::fs::read_dir(format!(
                "{}/tests/fixtures/{dir}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap()
        })
        .map(|e| serde_json::from_slice(&std::fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect();
    (
        prop::sample::select(fixtures),
        prop::collection::vec((prop::sample::select(KEYS), shaped()), 1..4),
    )
        .prop_map(|(mut v, edits)| {
            if let Value::Object(m) = &mut v {
                for (key, value) in edits {
                    m.insert(key.to_owned(), value);
                }
            }
            v
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn never_panics_on_bytes(body in prop::collection::vec(any::<u8>(), 0..256)) {
        decode(body);
    }

    #[test]
    fn never_panics_on_shaped_json(v in shaped()) {
        decode(serde_json::to_vec(&v).unwrap());
    }

    #[test]
    fn never_panics_on_mutated_fixtures(v in mutated_fixture()) {
        decode(serde_json::to_vec(&v).unwrap());
    }
}
