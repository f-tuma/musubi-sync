//! Read path of the sync core: account actors, scheduler, tenant writer,
//! entity merge, occurrence index and change bus.
//!
//! Skeleton from checkpoint K0; K5 implements it. The engine works against the
//! traits in `musubi-model` only: it must not depend on `musubi-store-sqlite` or
//! `musubi-google` outside `[dev-dependencies]`, and never on `musubi-google`.
