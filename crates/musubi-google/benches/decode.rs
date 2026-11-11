//! B8 (plan 6.5): `GoogleCodec::decode` per event over the golden fixtures,
//! reported and guarded against regression. `MUSUBI_BENCH_SMOKE=1` runs a
//! tenth of the rounds.
//!
//! Run: `cargo bench -p musubi-google --bench decode --locked`.

use std::hint::black_box;
use std::time::Instant;

use musubi_google::GoogleCodec;
use musubi_model::provider::Codec;
use musubi_model::remote::MediaType;
use musubi_model::{Event, Raw};

fn main() {
    let smoke = std::env::var_os("MUSUBI_BENCH_SMOKE").is_some();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/events");
    let raws: Vec<Raw> = std::fs::read_dir(dir)
        .expect("fixtures")
        .map(|e| Raw {
            media_type: MediaType::Json,
            body: std::fs::read(e.expect("entry").path()).expect("fixture"),
        })
        .collect();
    let rounds = if smoke { 2_000 } else { 20_000 };
    let t0 = Instant::now();
    for _ in 0..rounds {
        for r in &raws {
            black_box(<GoogleCodec as Codec<Event>>::decode(black_box(r)));
        }
    }
    let per = t0.elapsed().as_secs_f64() * 1e6 / (rounds * raws.len()) as f64;
    println!(
        "B8 codec: decode {per:.2} µs per event ({} fixtures × {rounds})",
        raws.len()
    );
    // shortcut: a fixed guard at ~10× the first measurement (K7), until a
    // dedicated runner keeps a baseline.
    assert!(
        per < GUARD_US,
        "B8 regression: {per:.2} µs per event, guard {GUARD_US} µs"
    );
}

const GUARD_US: f64 = 15.0;
