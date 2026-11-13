//! CalDAV and CardDAV [`musubi_model::Provider`] (docs/plan/phase2.md,
//! chapters 2 and 5).
//!
//! Skeleton of phase 2 K0: the crate exists so the dependency direction
//! (`musubi-model`, `musubi-ical`) and the licences of its dependencies are
//! checked from the start. K4 adds the HTTP client (the same `reqwest` as
//! `musubi-google`), WebDAV XML over `quick-xml`, discovery after RFC 6764,
//! deltas through `sync-collection` or ctag and etag, `multiget` and the
//! `generic` profile; K7 adds the `icloud` profile as data.
