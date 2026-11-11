//! Third-party code kept in tree. See each subdirectory's `README.md` for origin and changes.

// Upstream code is kept close to its original form so later diffs against upstream stay
// readable; unused upstream API and upstream style are therefore allowed here only.
#[allow(dead_code, unreachable_pub, clippy::all, clippy::pedantic)]
pub(crate) mod rrule;
