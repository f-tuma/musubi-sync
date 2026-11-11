# Vendored `rrule` core

- **Origin:** crate [`rrule`](https://crates.io/crates/rrule) 0.14.0, `src/` from the
  published tarball; repository <https://github.com/fmeringdal/rust-rrule>, commit
  `1c3420ede20e6190040804121e1b5e1eddac6493`.
- **Copyright:** (c) 2021 Fredrik Meringdal, Ralph Bisschops. Dual-licensed MIT or
  Apache-2.0; both licence texts are in this directory (`LICENSE-MIT`, `LICENSE-APACHE`),
  copied from the repository because the crate tarball does not ship them. Our changes are
  under the same terms.
- **Why vendored:** docs/research/phase0/recurrence.md (verdict, option b): the core's
  RRULE arithmetic is mature, but its time-zone layer (`chrono-tz`, `Tz::Local`) leaks the host
  zone and mishandles DST; we keep the arithmetic and replace the zone layer with our jiff edge
  in `../../lib.rs`.

## Changes against 0.14.0

Upstream code is otherwise unchanged apart from `cargo fmt`; changed spots carry a
`musubi:` comment.

1. **Zones removed.** `core/timezone.rs` and `core/timezone_impl.rs` are deleted;
   `core::Tz` is `chrono::Utc`. The core only ever sees civil time marked UTC (fix 1).
2. **No set layer.** `core/rruleset.rs`, `iter/rruleset_iter.rs` and the DTSTART/RDATE/EXDATE
   content-line parsers (`parser/content_line/{date,start_date}_content_line.rs`,
   `content_line_parts.rs`, `parser/regex.rs`) are deleted. The edge combines rules, RDATE,
   EXDATE and overrides itself (fixes 2-6, 9).
3. **RRULE value parser.** `parser::parse_rule(value)` parses one `FREQ=...` value and returns
   `UNTIL` raw; the edge converts it in the series zone (fix 7). `validate_until` and the
   `UNTIL` validation errors are removed for the same reason. `FromStr for RRule` and
   `RRule::build` are removed.
4. **Generation horizon.** `RRuleIter::stop_after` (with `DateTimeIter::period_starts_after`)
   ends iteration once a period starts after the edge's horizon, so a window does not walk the
   rest of an infinite series.
5. **Removed features:** `BYEASTER` (`iter/easter.rs`, the `by-easter` feature), the `serde`
   derives, `log`, `regex` (only the deleted content-line parsers used it) and
   `datetime_to_ical_format`; the `exrule` feature gate is gone (the edge applies EXRULE).
   `chrono` is used without its `clock` feature.
6. **Non-ASCII BYDAY.** `NWeekday::from_str` rejects a non-ASCII value instead of panicking
   on a char boundary (found by the fuzzer; upstream's comment claims a check it lacks).
7. **Module paths** are rewritten from `crate::` to `crate::vendor::rrule::`; the module is
   private to `musubi-recurrence`. Tests that depended on removed code (named zones, `Tz::LOCAL`,
   `UNTIL` validation, DTSTART parsing) are dropped; the remaining upstream unit tests run with
   the crate's tests. Two doc examples are marked `ignore` because the module is private.
