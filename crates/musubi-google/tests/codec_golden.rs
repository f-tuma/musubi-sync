//! Golden fixtures for `GoogleCodec` (plan K4): one snapshot per Google shape.

use musubi_google::GoogleCodec;
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{CalendarEvent, Raw};

fn decode(name: &str) -> CalendarEvent {
    let path = format!(
        "{}/tests/fixtures/events/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let body = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    match GoogleCodec::decode(&Raw {
        media_type: MediaType::Json,
        body,
    }) {
        Decoded::Canonical(event) => event,
        Decoded::Unsupported { reason } => panic!("{name}: unsupported: {reason}"),
        _ => panic!("{name}: unexpected decode result"),
    }
}

macro_rules! golden {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            insta::assert_json_snapshot!(decode(stringify!($name)));
        }
    )*};
}

golden!(
    timed,
    all_day,
    series_master,
    series_exception,
    cancelled_occurrence,
    foreign_zone,
    unknown_field,
);

#[test]
fn exception_joins_its_master_by_uid_and_recurrence_id() {
    let master = decode("series_master");
    for name in ["series_exception", "cancelled_occurrence"] {
        let exception = decode(name);
        assert_eq!(exception.uid, master.uid, "{name}");
        let rid = exception
            .recurrence_id
            .expect("exception has a recurrence id");
        assert_eq!(
            rid.time(),
            master.start.time(),
            "{name}: an occurrence of the series"
        );
        assert_eq!(
            exception.recurrence_id_time_zone, master.time_zone,
            "{name}"
        );
    }
}
