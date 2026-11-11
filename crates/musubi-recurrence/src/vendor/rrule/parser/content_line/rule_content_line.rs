use std::{collections::HashMap, marker::PhantomData, str::FromStr};

use chrono::Weekday;

use crate::vendor::rrule::{
    Frequency, RRule, Unvalidated,
    parser::{
        ParseError, content_line::parameters::parse_parameters, datetime::parse_weekdays,
        str_to_weekday, utils::parse_str_to_vec,
    },
};

#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy)]
pub enum RRuleProperty {
    Freq,
    Until,
    Count,
    Interval,
    BySecond,
    ByMinute,
    ByHour,
    ByDay,
    ByMonthDay,
    ByYearDay,
    ByWeekNo,
    ByMonth,
    BySetPos,
    Wkst,
}

impl FromStr for RRuleProperty {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let prop = match &s.to_uppercase()[..] {
            "FREQ" => Self::Freq,
            "UNTIL" => Self::Until,
            "COUNT" => Self::Count,
            "INTERVAL" => Self::Interval,
            "BYSECOND" => Self::BySecond,
            "BYMINUTE" => Self::ByMinute,
            "BYHOUR" => Self::ByHour,
            "BYWEEKDAY" | "BYDAY" => Self::ByDay,
            "BYMONTHDAY" => Self::ByMonthDay,
            "BYYEARDAY" => Self::ByYearDay,
            "BYWEEKNO" => Self::ByWeekNo,
            "BYMONTH" => Self::ByMonth,
            "BYSETPOS" => Self::BySetPos,
            "WKST" => Self::Wkst,
            _ => return Err(ParseError::UnrecognizedParameter(s.into())),
        };
        Ok(prop)
    }
}

/// musubi: parses one RRULE/EXRULE *value* (`FREQ=...;...`, no property name or parameters).
/// `UNTIL` is returned raw: the jiff edge converts it, because its meaning depends on the
/// series zone, which the core never sees.
pub(crate) fn parse_rule(value: &str) -> Result<(RRule<Unvalidated>, Option<String>), ParseError> {
    let properties: HashMap<RRuleProperty, String> = parse_parameters(value)?;
    let until = properties.get(&RRuleProperty::Until).cloned();
    Ok((props_to_rrule(&properties)?, until))
}

/// Takes a map of [`RRuleProperty`] and returns an [`RRule`].
#[allow(clippy::too_many_lines)]
fn props_to_rrule(
    props: &HashMap<RRuleProperty, String>,
) -> Result<RRule<Unvalidated>, ParseError> {
    let freq = props
        .get(&RRuleProperty::Freq)
        .map(|freq| Frequency::from_str(freq))
        .transpose()?
        .ok_or_else(|| ParseError::MissingProperty("FREQ".into()))?;
    let interval = props
        .get(&RRuleProperty::Interval)
        .map(|interval| {
            interval
                .parse()
                .map_err(|_| ParseError::InvalidInterval(interval.into()))
        })
        .transpose()?
        .unwrap_or(1);
    let count = props
        .get(&RRuleProperty::Count)
        .map(|count| {
            count
                .parse()
                .map_err(|_| ParseError::InvalidCount(count.into()))
        })
        .transpose()?;
    let until = None; // musubi: see `parse_rule`
    let week_start = props
        .get(&RRuleProperty::Wkst)
        .map(|week_start| {
            str_to_weekday(week_start)
                .map_err(|_| ParseError::InvalidWeekdayStart(week_start.into()))
        })
        .transpose()?
        .unwrap_or(Weekday::Mon);
    let by_set_pos = props
        .get(&RRuleProperty::BySetPos)
        .map(|by_set_pos| {
            parse_str_to_vec(by_set_pos, |_| true)
                .map_err(|_| ParseError::InvalidBySetPos(by_set_pos.into()))
        })
        .transpose()?
        .unwrap_or_default();
    let by_month = props
        .get(&RRuleProperty::ByMonth)
        .map(|by_month| {
            parse_str_to_vec(by_month, |month| (1..=12).contains(&month))
                .map_err(|_| ParseError::InvalidByMonth(by_month.into()))
        })
        .transpose()?
        .unwrap_or_default();
    let by_month_day = props
        .get(&RRuleProperty::ByMonthDay)
        .map(|by_month_day| {
            parse_str_to_vec(by_month_day, |monthday| (-31..=31).contains(&monthday))
                .map_err(|_| ParseError::InvalidByMonthDay(by_month_day.into()))
        })
        .transpose()?
        .unwrap_or_default();
    let by_year_day = props
        .get(&RRuleProperty::ByYearDay)
        .map(|by_year_day| {
            parse_str_to_vec(by_year_day, |yearday| (-366..=366).contains(&yearday))
                .map_err(|_| ParseError::InvalidByYearDay(by_year_day.into()))
        })
        .transpose()?
        .unwrap_or_default();
    let by_week_no = props
        .get(&RRuleProperty::ByWeekNo)
        .map(|by_week_no| {
            parse_str_to_vec(by_week_no, |weekno| (-53..=53).contains(&weekno))
                .map_err(|_| ParseError::InvalidByWeekNo(by_week_no.into()))
        })
        .transpose()?
        .unwrap_or_default();
    let by_weekday = props
        .get(&RRuleProperty::ByDay)
        .map(|by_weekday| parse_weekdays(by_weekday))
        .transpose()?
        .unwrap_or_default();
    let by_hour = props
        .get(&RRuleProperty::ByHour)
        .map(|by_hour| {
            parse_str_to_vec(by_hour, |hour| hour < 24)
                .map_err(|_| ParseError::InvalidByHour(by_hour.into()))
        })
        .transpose()?
        .unwrap_or_default();
    let by_minute = props
        .get(&RRuleProperty::ByMinute)
        .map(|by_minute| {
            parse_str_to_vec(by_minute, |minute| minute < 60)
                .map_err(|_| ParseError::InvalidByMinute(by_minute.into()))
        })
        .transpose()?
        .unwrap_or_default();
    let by_second = props
        .get(&RRuleProperty::BySecond)
        .map(|by_second| {
            parse_str_to_vec(by_second, |second| second < 60)
                .map_err(|_| ParseError::InvalidBySecond(by_second.into()))
        })
        .transpose()?
        .unwrap_or_default();

    // Check if mandatory fields are set
    Ok(RRule {
        freq,
        interval,
        count,
        until,
        week_start,
        by_set_pos,
        by_month,
        by_month_day,
        by_n_month_day: vec![],
        by_year_day,
        by_week_no,
        by_weekday,
        by_hour,
        by_minute,
        by_second,
        stage: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rrule_value() {
        let tests = [
            (
                "FREQ=DAILY",
                RRule {
                    freq: Frequency::Daily,
                    ..Default::default()
                },
            ),
            (
                "BYHOUR=4;FREQ=DAILY",
                RRule {
                    by_hour: vec![4],
                    freq: Frequency::Daily,
                    ..Default::default()
                },
            ),
            (
                // Testing case insensitivity
                "byhour=4;freQ=DAILY",
                RRule {
                    by_hour: vec![4],
                    freq: Frequency::Daily,
                    ..Default::default()
                },
            ),
        ];

        for (input, expected_output) in tests {
            assert_eq!(parse_rule(input), Ok((expected_output, None)));
        }
    }

    #[test]
    fn returns_until_raw() {
        let (_, until) = parse_rule("FREQ=DAILY;UNTIL=20260101T000000Z").unwrap();
        assert_eq!(until.as_deref(), Some("20260101T000000Z"));
    }

    #[test]
    fn rejects_missing_freq_and_unknown_parts() {
        assert!(parse_rule("COUNT=3").is_err());
        assert!(parse_rule("FREQ=DAILY;FOO=1").is_err());
        assert!(parse_rule("FREQ=DAILY;COUNT=1;COUNT=2").is_err());
    }
}
