//! The workspace pins jiff to its bundled tzdb (root Cargo.toml): zone lookups
//! must work without the host's zoneinfo.

#[test]
fn bundled_tzdb_resolves_iana_zones() {
    let tz = jiff::tz::TimeZone::get("Europe/Prague").expect("bundled tzdb");
    let at: jiff::Timestamp = "2026-07-01T12:00:00Z".parse().unwrap();
    assert_eq!(tz.to_offset(at).seconds(), 2 * 3600);
}
