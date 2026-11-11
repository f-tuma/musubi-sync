mod datetime;
mod rrule;

pub use self::rrule::{Frequency, NWeekday, RRule};
pub(crate) use datetime::{
    duration_from_midnight, get_day, get_hour, get_minute, get_month, get_second,
};
/// musubi: the core only ever sees civil time marked UTC; zones are resolved by the jiff edge
/// (replaces upstream's `Tz` enum over `chrono-tz` and the host's local zone).
pub type Tz = chrono::Utc;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
/// An empty struct to keep the validated stage
pub struct Validated;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
/// An empty struct to keep the unvalidated (or not-yet-validated) stage
pub struct Unvalidated;
