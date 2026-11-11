//! `musubi-syncd`: the server binary (`serve`, `backup`, `restore`).
//!
//! Skeleton from checkpoint K0; K6 implements it. This is the only crate that
//! wires the SQLite store and the Google adapter into the engine.

// Linked so that the dependency graph matches docs/plan/phase1.md chapter 2.
use musubi_engine as _;
use musubi_google as _;
use musubi_model as _;
use musubi_recurrence as _;
use musubi_store_sqlite as _;

fn main() {
    eprintln!("musubi-syncd: not implemented yet (phase 1, checkpoint K6)");
    std::process::exit(2);
}
