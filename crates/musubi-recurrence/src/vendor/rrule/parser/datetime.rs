use super::ParseError;
use crate::vendor::rrule::NWeekday;
use chrono::Weekday;

/// Attempts to convert a `str` to a `Weekday`.
pub(crate) fn str_to_weekday(d: &str) -> Result<Weekday, ParseError> {
    let day = match &d.to_uppercase()[..] {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return Err(ParseError::InvalidWeekday(d.to_string())),
    };
    Ok(day)
}

/// Parse the "BYWEEKDAY" and "BYDAY" values
/// Example: `SU,MO,TU,WE,TH,FR` or `4MO` or `-1WE`
/// > For example, within a MONTHLY rule, +1MO (or simply 1MO) represents the first Monday
/// > within the month, whereas -1MO represents the last Monday of the month.
pub(crate) fn parse_weekdays(val: &str) -> Result<Vec<NWeekday>, ParseError> {
    let mut wdays = vec![];
    // Separate all days
    for day in val.split(',') {
        let wday = day.parse::<NWeekday>()?;
        wdays.push(wday);
    }
    Ok(wdays)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_nweekdays() {
        let tests = [
            ("SU", vec![NWeekday::Every(Weekday::Sun)]),
            ("-12TU", vec![NWeekday::Nth(-12, Weekday::Tue)]),
            (
                "MO,WE",
                vec![NWeekday::Every(Weekday::Mon), NWeekday::Every(Weekday::Wed)],
            ),
            (
                "MO,WE,3TU,-4SA",
                vec![
                    NWeekday::Every(Weekday::Mon),
                    NWeekday::Every(Weekday::Wed),
                    NWeekday::Nth(3, Weekday::Tue),
                    NWeekday::Nth(-4, Weekday::Sat),
                ],
            ),
        ];

        for (input, expected_output) in tests {
            let output = parse_weekdays(input);
            assert_eq!(output, Ok(expected_output));
        }
    }

    #[test]
    fn rejects_invalid_nweekdays() {
        let tests = ["", "    ", "fjoasfjapsjop", "MONDAY", "MONDAY, TUESDAY"];

        for input in tests {
            let res = parse_weekdays(input);
            assert!(res.is_err());
        }
    }

    #[test]
    fn parses_valid_weekdays() {
        let tests = [
            ("MO", Weekday::Mon),
            ("TU", Weekday::Tue),
            ("WE", Weekday::Wed),
            ("TH", Weekday::Thu),
            ("FR", Weekday::Fri),
            ("SA", Weekday::Sat),
            ("SU", Weekday::Sun),
        ];

        for (input, expected_output) in tests {
            let output = str_to_weekday(input);
            assert_eq!(output, Ok(expected_output));
        }
    }

    #[test]
    fn rejects_invalid_weekdays() {
        let tests = ["", "    ", "fjoasfjapsjop", "MONDAY", "MONDAY, TUESDAY"];

        for input in tests {
            let res = str_to_weekday(input);
            assert!(res.is_err());
        }
    }
}
