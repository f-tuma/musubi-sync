//! D3 (plan 11.2): decode throughput of `musubi-ical` in MB/s for VEVENT,
//! VTODO and vCard over the golden fixtures. Measured and reported only, no
//! guard. `MUSUBI_BENCH_SMOKE=1` runs a tenth of the rounds.
//!
//! Run: `cargo bench -p musubi-ical --bench decode --locked`.

use std::hint::black_box;
use std::time::Instant;

use musubi_ical::{IcalCodec, VCardCodec};
use musubi_model::provider::Codec;
use musubi_model::remote::MediaType;
use musubi_model::{Contact, Event, Raw, Task};

fn load(dir: &str, media_type: MediaType) -> Vec<Raw> {
    let dir = format!("{}/tests/fixtures/{dir}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_dir(dir)
        .expect("fixtures")
        .map(|e| Raw {
            media_type,
            body: std::fs::read(e.expect("entry").path()).expect("fixture"),
        })
        .collect()
}

fn measure<T>(label: &str, raws: &[Raw], rounds: usize, decode: impl Fn(&Raw) -> T) {
    let bytes: usize = raws.iter().map(|r| r.body.len()).sum();
    let t0 = Instant::now();
    for _ in 0..rounds {
        for r in raws {
            black_box(decode(black_box(r)));
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let mbs = (bytes * rounds) as f64 / secs / 1e6;
    let per = secs * 1e6 / (rounds * raws.len()) as f64;
    println!(
        "D3 {label}: {mbs:.1} MB/s, {per:.2} µs per object ({} fixtures × {rounds})",
        raws.len()
    );
}

fn main() {
    let smoke = std::env::var_os("MUSUBI_BENCH_SMOKE").is_some();
    let rounds = if smoke { 1_000 } else { 10_000 };
    let events = load("events", MediaType::Calendar);
    let tasks = load("tasks", MediaType::Calendar);
    let cards = load("contacts", MediaType::VCard);
    measure(
        "VEVENT",
        &events,
        rounds,
        <IcalCodec as Codec<Event>>::decode,
    );
    measure("VTODO", &tasks, rounds, <IcalCodec as Codec<Task>>::decode);
    measure(
        "vCard",
        &cards,
        rounds,
        <VCardCodec as Codec<Contact>>::decode,
    );
}
