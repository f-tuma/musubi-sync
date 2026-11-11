use super::Tz;
use chrono::{Datelike, Duration, NaiveTime, Timelike};

pub(crate) fn duration_from_midnight(time: NaiveTime) -> Duration {
    Duration::hours(i64::from(time.hour()))
        + Duration::minutes(i64::from(time.minute()))
        + Duration::seconds(i64::from(time.second()))
}

pub(crate) fn get_month(dt: &chrono::DateTime<Tz>) -> u8 {
    u8::try_from(dt.month()).expect("month is between 1-12 which is covered by u8")
}

pub(crate) fn get_day(dt: &chrono::DateTime<Tz>) -> i8 {
    i8::try_from(dt.day()).expect("day is between 1-31 which is covered by i8")
}

pub(crate) fn get_hour(dt: &chrono::DateTime<Tz>) -> u8 {
    u8::try_from(dt.hour()).expect("hour is between 0-23 which is covered by u8")
}

pub(crate) fn get_minute(dt: &chrono::DateTime<Tz>) -> u8 {
    u8::try_from(dt.minute()).expect("minute is between 0-59 which is covered by u8")
}

pub(crate) fn get_second(dt: &chrono::DateTime<Tz>) -> u8 {
    u8::try_from(dt.second()).expect("second is between 0-59 which is covered by u8")
}
