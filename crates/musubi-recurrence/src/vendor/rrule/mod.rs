//! Vendored core of the [`rrule`](https://crates.io/crates/rrule) crate, version 0.14.0
//! (<https://github.com/fmeringdal/rust-rrule>, commit `1c3420ede20e6190040804121e1b5e1eddac6493`).
//!
//! Copyright (c) 2021 Fredrik Meringdal, Ralph Bisschops. Licensed under MIT or Apache-2.0,
//! see `LICENSE-MIT` and `LICENSE-APACHE` in this directory; changes are listed in `README.md`.
//!
//! Private to `musubi-recurrence`: the iterator here is unbounded and zone-unaware, the only way
//! in is the crate's windowed, capped `expand`.

mod core;
mod error;
mod iter;
mod parser;
mod validator;

pub(crate) use crate::vendor::rrule::core::{Frequency, NWeekday, RRule, Tz};
pub(crate) use crate::vendor::rrule::core::{Unvalidated, Validated};
pub(crate) use error::RRuleError;
pub(crate) use iter::RRuleIter;
pub(crate) use parser::parse_rule;
