//! Golden fixtures for `TasksCodec` and `PeopleCodec` (plan K6).
//!
//! The fixtures are synthetic, built from the documented `Task` and `Person`
//! shapes; T10's recordings replace them file for file.

use musubi_google::{PeopleCodec, TasksCodec};
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{Contact, Kind, Raw, Task};

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

golden!(Task, TasksCodec, "tasks": tasks_open, tasks_completed, tasks_hidden, tasks_subtask, tasks_unknown_field);
golden!(
    Contact,
    PeopleCodec,
    "people": people_full,
    people_minimal,
    people_email_only,
    people_birthday_without_year,
    people_unknown_field,
);
