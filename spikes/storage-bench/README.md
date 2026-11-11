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
./run.sh            # full run, ~40 min; results/*.md + run.log; removes container + data afterwards
KEEP=1 ./run.sh     # keep the container and data/ for poking around
SKIP_PG=1 ./run.sh  # SQLite phases, A/B switches and acceptmix only; pg.md and backup.md stay as they are
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

`run.sh` refuses a non-empty `BENCH_DATA` without its `.storage-bench-data` marker, and removes the
container and data on exit (also on failure) unless `KEEP=1`. Its full log is `results/run.log`.

`.cargo/config.toml` builds the bundled SQLite with `SQLITE_DEFAULT_MEMSTATUS=0`. Without it, a
process-wide malloc mutex serializes readers on different connections. A/B switch: `SQLITE_MEMSTATUS=1`
turns it back on at runtime (`results/ab-memstatus.md`; factor: see the report, section 3.4).

WAL checkpoints (`src/sqlite.rs`, A/B switch `SQLITE_CKPT`, results in `results/ab-ckpt-*.md`):

- `bg` (default): a background thread runs `wal_checkpoint(PASSIVE)` when a tenant's WAL reaches 8 MB
  (smaller ones once a second). PASSIVE never resets the WAL while readers or a busy writer keep using
  it, so the tenant's writer runs `wal_checkpoint(TRUNCATE)` after its commit once the WAL reaches
  64 MB (`SQLITE_WAL_HARD_MB`), waiting at most 10 ms for readers. If a reader still holds old frames,
  the writer does not retry for 1 s or until the WAL grows by another 32 MB. The outbox accept never
  runs it (it must not wait on a checkpoint). `journal_size_limit=4MB`,
  `wal_autocheckpoint=65536` as a fuse. Writers use `BEGIN IMMEDIATE`, so they wait on the busy handler
  instead of failing with `SQLITE_BUSY` during a checkpoint. Result notes give the maximum WAL size.
- `inline`: SQLite default `wal_autocheckpoint=1000` in the committing writer (the thread only samples the WAL size).
- `passive`: the first version, PASSIVE every 50 ms with `wal_autocheckpoint=0`. Kept only to reproduce
  the unbounded WAL growth under readers.

`sqlite acceptmix` (`results/acceptmix.md`): the tenant's single writer ingests the large tenant
(page 250, initial load + 2 update rounds) and runs queued outbox accepts (~50/s) before each next page;
accept latency is enqueue -> commit. Second pass adds a reader that holds a snapshot for 1.5 s, 0.2 s apart.

The release profile has `panic = "abort"`: a panic in any thread, the checkpointer included, fails the run.

`size_zstd.py <tenant.db>...` (Python 3.14+, stdlib) measures a tenant file with `raw.body`
compressed by zstd, per row and with a per-tenant dictionary (`results/size-zstd.md`).
