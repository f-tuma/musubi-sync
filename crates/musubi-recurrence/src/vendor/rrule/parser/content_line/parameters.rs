use std::{collections::HashMap, hash::Hash, str::FromStr};

use crate::vendor::rrule::parser::ParseError;

/// Parses a string of semicolon seperated key value pairs into a `HashMap` with
/// predefined keys. It will return an error if duplicate keys are found.
pub(super) fn parse_parameters<K: FromStr<Err = ParseError> + Hash + Eq>(
    raw_parameters: &str,
) -> Result<HashMap<K, String>, ParseError> {
    let mut parameters = HashMap::new();
    for raw_parameter in raw_parameters.split(';') {
        if raw_parameter.is_empty() {
            continue;
        }
        let (raw_parameter, value) = raw_parameter
            .split_once('=')
            .ok_or_else(|| ParseError::InvalidParameterFormat(raw_parameter.into()))?;
        let parameter = K::from_str(raw_parameter)?;

        if parameters.insert(parameter, value.into()).is_some() {
            return Err(ParseError::DuplicateProperty(raw_parameter.into()));
        }
    }
    Ok(parameters)
}

#[cfg(test)]
mod tests {
    use super::super::rule_content_line::RRuleProperty as DateParameter;

    use super::*;

    #[test]
    fn parses_valid_property_parameters() {
        let tests = [
            (
                "FREQ=DAILY",
                [(DateParameter::Freq, "DAILY".to_string())]
                    .into_iter()
                    .collect::<HashMap<_, _>>(),
            ),
            (
                "BYHOUR=4;COUNT=2",
                [
                    (DateParameter::ByHour, "4".to_string()),
                    (DateParameter::Count, "2".to_string()),
                ]
                .into_iter()
                .collect::<HashMap<_, _>>(),
            ),
        ];

        for (input, expected_output) in tests {
            let output = parse_parameters(input);
            assert_eq!(output, Ok(expected_output));
        }
    }

    #[test]
    fn rejecets_unrecognized_property_parameters() {
        let tests = [(
            "VALUES=DATE",
            ParseError::UnrecognizedParameter("VALUES".into()),
        )];

        for (input, expected_output) in tests {
            let output: Result<HashMap<DateParameter, String>, _> = parse_parameters(input);
            assert_eq!(output, Err(expected_output));
        }
    }

    #[test]
    fn rejecets_malformed_property_parameters() {
        let tests = [(
            "VALUE:DATE",
            ParseError::InvalidParameterFormat("VALUE:DATE".into()),
        )];

        for (input, expected_output) in tests {
            let output: Result<HashMap<DateParameter, String>, _> = parse_parameters(input);
            assert_eq!(output, Err(expected_output));
        }
    }

    #[test]
    fn rejecets_duplicate_parameters() {
        let tests = [(
            "FREQ=DAILY;COUNT=1;COUNT=2",
            ParseError::DuplicateProperty("COUNT".into()),
        )];

        for (input, expected_output) in tests {
            let output: Result<HashMap<DateParameter, String>, _> = parse_parameters(input);
            assert_eq!(output, Err(expected_output));
        }
    }

    #[test]
    fn does_not_attempt_to_parse_empty_parameters() {
        let tests = [
            ("", [].into_iter().collect::<HashMap<_, _>>()),
            (";", [].into_iter().collect::<HashMap<_, _>>()),
        ];

        for (input, expected_output) in tests {
            let output: Result<HashMap<DateParameter, String>, _> = parse_parameters(input);
            assert_eq!(output, Ok(expected_output));
        }
    }
}
