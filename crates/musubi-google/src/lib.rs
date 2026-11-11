//! Google Calendar adapter: `Provider`, `PushSource` and `Codec<Event>`.
//!
//! Skeleton from checkpoint K0; K4 implements it (docs/plan/phase1.md, chapter 4).
//! The adapter knows neither the engine nor the storage. Only `musubi-syncd`
//! and tests may depend on this crate.
