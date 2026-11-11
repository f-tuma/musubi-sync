# storage-bench

Phase 0 spike: SQLite-per-tenant (WAL) vs shared PostgreSQL 17 for the musubi-sync data plane
(proposal ch. 4). Report: [`docs/research/phase0/storage.md`](../../docs/research/phase0/storage.md).

The same minimal schema runs on both backends: `items` (raw ref, canonical, remote key + validator,
rev, seq, tombstone), `entities` + `placements` (ch. 12), `occurrences` (index), `changes`
(latest change per entity, monotonic seq per tenant), `mutations` and `outbox`.
Data comes from `src/datagen.rs`, which is deterministic: the same seed gives the same items on both backends.

## Run

Needs Docker (pulls `postgres:17`) and Rust 1.85 or newer.

```sh
./run.sh            # full run, ~25 min; results/ *.md; removes container + data afterwards
KEEP=1 ./run.sh     # keep the container and data/ for poking around
```

Single phases (the PG container must be running; see `run.sh`):

```sh
cargo build --release
./target/release/storage-bench sqlite load size read mixed outbox coldopen
./target/release/storage-bench pg     load size read mixed outbox connect
```

Environment knobs: `BENCH_DATA` (default `./data`), `PG_URL`, `TENANTS=200`, `EVENTS=5000`,
`LARGE=100000`, `CONC=16`, `COLD=10000`, `QUERIES=2000`, `MIXED_SECS=20`.
For a quick smoke run: `TENANTS=20 EVENTS=500 LARGE=3000 CONC=4 COLD=200 QUERIES=50 MIXED_SECS=2`.

`.cargo/config.toml` builds the bundled SQLite with `SQLITE_DEFAULT_MEMSTATUS=0`. Without it, a
process-wide malloc mutex serializes readers on different connections, and 8 reader threads drop
about 8× in throughput (see the report).

SQLite runs with `wal_autocheckpoint=0` and a background `Checkpointer` (`src/sqlite.rs`) that runs
`wal_checkpoint(PASSIVE)` every 50 ms. The default inline auto-checkpoint costs the committing writer
25–40 ms on this disk and cut ingest throughput 3–4×.
