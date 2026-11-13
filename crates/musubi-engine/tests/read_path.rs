//! Acceptance tests of the read path (plan ch. 5, K5) and of the phase 2
//! engine (plan ch. 3.4-3.7, 7, 10; K5): the engine against the SQLite store
//! on a temporary directory and the scripted `FakeProvider`.
//!
//! The cases live in `cases/`; `registry.rs` runs them again as another
//! provider.

const SECOND: bool = false;

include!("cases/harness.rs");
include!("cases/read_path.rs");
include!("cases/k5.rs");
