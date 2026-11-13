//! The iCalendar and vCard codecs are total and deterministic (plan K2,
//! chapter 8.4): no panic on arbitrary bytes or mutated fixtures, and the
//! same input always gives the same result.

use musubi_ical::{IcalCodec, VCardCodec};
use musubi_model::provider::Codec;
use musubi_model::remote::MediaType;
use musubi_model::{Contact, ContactGroup, Event, Raw, Task};
use proptest::prelude::*;

/// Decodes `body` as every kind and checks determinism.
fn decode_all(body: Vec<u8>) {
    let cal = Raw {
        media_type: MediaType::Calendar,
        body: body.clone(),
    };
    let card = Raw {
        media_type: MediaType::VCard,
        body,
    };
    assert_eq!(
        <IcalCodec as Codec<Event>>::decode(&cal),
        <IcalCodec as Codec<Event>>::decode(&cal)
    );
    assert_eq!(
        <IcalCodec as Codec<Task>>::decode(&cal),
        <IcalCodec as Codec<Task>>::decode(&cal)
    );
    assert_eq!(
        <VCardCodec as Codec<Contact>>::decode(&card),
        <VCardCodec as Codec<Contact>>::decode(&card)
    );
    assert_eq!(
        <VCardCodec as Codec<ContactGroup>>::decode(&card),
        <VCardCodec as Codec<ContactGroup>>::decode(&card)
    );
}

fn fixtures() -> Vec<String> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut out = Vec::new();
    for dir in ["events", "tasks", "contacts"] {
        for entry in std::fs::read_dir(format!("{root}/{dir}")).unwrap() {
            out.push(std::fs::read_to_string(entry.unwrap().path()).unwrap());
        }
    }
    out.sort();
    out
}

/// Values that almost parse as iCalendar and vCard dates, durations, zones
/// and rules.
fn junk() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        ".{0,16}",
        prop_oneof![
            Just("99991231T235959Z"),
            Just("00000101T000000"),
            Just("20261332T256161"),
            Just("20261116"),
            Just("-P9999999W"),
            Just("P99999999999D"),
            Just("PT-1S"),
            Just("+9959"),
            Just("-000001"),
            Just("FREQ=SECONDLY;INTERVAL=0"),
            Just("FREQ=YEARLY;BYMONTH=13;BYDAY=9SU"),
            Just("FREQ=MONTHLY;BYDAY=-5SU;UNTIL=00010101"),
            Just("mailto:"),
            Just("urn:uuid:"),
            Just(";;;;;;;;"),
            Just("--0229"),
            Just("1604-02-30"),
        ]
        .prop_map(String::from),
    ]
}

#[derive(Debug, Clone)]
enum Mutation {
    Delete(usize),
    Duplicate(usize),
    Swap(usize, usize),
    /// Replaces the value after the first `:` of a line.
    Value(usize, String),
    /// Replaces the parameters between the name and the first `:`.
    Params(usize, String),
    Truncate(usize),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        any::<usize>().prop_map(Mutation::Delete),
        any::<usize>().prop_map(Mutation::Duplicate),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Mutation::Swap(a, b)),
        (any::<usize>(), junk()).prop_map(|(i, v)| Mutation::Value(i, v)),
        (any::<usize>(), junk()).prop_map(|(i, v)| Mutation::Params(i, format!(";{v}"))),
        any::<usize>().prop_map(Mutation::Truncate),
    ]
}

fn mutate(text: &str, mutations: &[Mutation]) -> Vec<u8> {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    for m in mutations {
        if lines.is_empty() {
            break;
        }
        let n = lines.len();
        match m {
            Mutation::Delete(i) => {
                lines.remove(i % n);
            }
            Mutation::Duplicate(i) => {
                let line = lines[i % n].clone();
                lines.insert(i % n, line);
            }
            Mutation::Swap(a, b) => lines.swap(a % n, b % n),
            Mutation::Value(i, v) => {
                let line = &mut lines[i % n];
                if let Some(at) = line.find(':') {
                    line.truncate(at + 1);
                    line.push_str(v);
                }
            }
            Mutation::Params(i, p) => {
                let line = &mut lines[i % n];
                if let Some(at) = line.find(':') {
                    let name_end = line[..at].find(';').unwrap_or(at);
                    line.replace_range(name_end..at, p);
                }
            }
            Mutation::Truncate(i) => {
                let line = &mut lines[i % n];
                let mut cut = i % (line.len() + 1);
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
            }
        }
    }
    lines.join("\r\n").into_bytes()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_bytes_never_panic(body in prop::collection::vec(any::<u8>(), 0..512)) {
        decode_all(body);
    }

    #[test]
    fn mutated_fixtures_never_panic(
        index in any::<usize>(),
        mutations in prop::collection::vec(mutation(), 1..8),
    ) {
        let fixtures = fixtures();
        decode_all(mutate(&fixtures[index % fixtures.len()], &mutations));
    }
}

#[test]
fn fixtures_decode_deterministically() {
    for f in fixtures() {
        decode_all(f.into_bytes());
    }
}
