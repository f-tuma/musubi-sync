//! `GoogleCodec::decode` is total: no panic on any input (plan K4).

use musubi_google::GoogleCodec;
use musubi_model::Raw;
use musubi_model::provider::Codec;
use musubi_model::remote::MediaType;
use proptest::prelude::*;
use serde_json::{Map, Value};

fn decode(body: Vec<u8>) {
    let _ = GoogleCodec::decode(&Raw {
        media_type: MediaType::Json,
        body,
    });
}

fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        ".{0,12}".prop_map(Value::from),
        // Strings that almost parse as Google dates, times, zones and rules.
        prop_oneof![
            Just("2026-11-12"),
            Just("2026-11-12T10:00:00+01:00"),
            Just("2026-11-12T10:00:00"),
            Just("-009999-01-01T00:00:00Z"),
            Just("9999-12-31T23:59:59-25:59"),
            Just("Europe/Prague"),
            Just("UTC"),
            Just("cancelled"),
            Just("RRULE:FREQ=DAILY"),
            Just("EXDATE;TZID=Europe/Prague:20261102T090000"),
            Just("EXDATE;VALUE=DATE:99991231"),
            Just("RDATE:20261201T100000Z/PT1H"),
            Just("EXDATE:"),
        ]
        .prop_map(Value::from),
    ]
}

fn json() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map(".{0,8}", inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

const KEYS: &[&str] = &[
    "id",
    "status",
    "iCalUID",
    "recurringEventId",
    "originalStartTime",
    "summary",
    "description",
    "location",
    "start",
    "end",
    "endTimeUnspecified",
    "recurrence",
    "transparency",
    "visibility",
    "organizer",
    "attendees",
    "reminders",
    "created",
    "updated",
    "sequence",
    "date",
    "dateTime",
    "timeZone",
    "email",
    "optional",
    "organizer",
    "responseStatus",
    "useDefault",
    "overrides",
    "method",
    "minutes",
];

/// Objects built from the field names the codec reads, at any depth.
fn event_shaped() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((prop::sample::select(KEYS), inner), 0..10).prop_map(|kv| {
                Value::Object(
                    kv.into_iter()
                        .map(|(k, v)| (k.to_owned(), v))
                        .collect::<Map<_, _>>(),
                )
            }),
        ]
    })
}

/// A golden fixture with some fields (top level or inside `start`/`end`/
/// `originalStartTime`) replaced by arbitrary values, so every branch of the
/// codec sees hostile input past the first type check.
fn mutated_fixture() -> impl Strategy<Value = Value> {
    let fixtures: Vec<Value> = std::fs::read_dir(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/events"
    ))
    .unwrap()
    .map(|e| serde_json::from_slice(&std::fs::read(e.unwrap().path()).unwrap()).unwrap())
    .collect();
    let nested = prop::sample::select(&["", "start", "end", "originalStartTime"][..]);
    (
        prop::sample::select(fixtures),
        prop::collection::vec((nested, prop::sample::select(KEYS), event_shaped()), 1..4),
    )
        .prop_map(|(mut v, edits)| {
            for (parent, key, value) in edits {
                let target = if parent.is_empty() {
                    Some(&mut v)
                } else {
                    v.get_mut(parent)
                };
                if let Some(Value::Object(m)) = target {
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
    fn never_panics_on_json(v in json()) {
        decode(serde_json::to_vec(&v).unwrap());
    }

    #[test]
    fn never_panics_on_event_shaped_json(v in event_shaped()) {
        decode(serde_json::to_vec(&v).unwrap());
    }

    #[test]
    fn never_panics_on_mutated_fixtures(v in mutated_fixture()) {
        decode(serde_json::to_vec(&v).unwrap());
    }
}
