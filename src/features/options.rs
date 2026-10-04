//! Typed access to feature parameters, hiding their configuration representation.

use serde_json::Value;
use snafu::Snafu;
use std::collections::HashMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureParameters {
    values: HashMap<String, Value>,
}

#[derive(Debug, PartialEq, Eq, Snafu)]
pub enum FeatureOptionsError {
    #[snafu(display("option '{name}' must be {expected}"))]
    InvalidType {
        name: String,
        expected: &'static str,
    },
}

impl From<HashMap<String, Value>> for FeatureParameters {
    fn from(values: HashMap<String, Value>) -> Self {
        Self { values }
    }
}

impl FeatureParameters {
    pub fn boolean(&self, name: &str, default: bool) -> Result<bool, FeatureOptionsError> {
        let Some(value) = self.values.get(name) else {
            return Ok(default);
        };

        value
            .as_bool()
            .ok_or_else(|| FeatureOptionsError::InvalidType {
                name: name.into(),
                expected: "a boolean",
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_parameters_use_the_callers_default_and_explicit_false_overrides_true() {
        let parameters = FeatureParameters::default();

        assert!(!parameters.boolean("enabled", false).unwrap());
        assert!(parameters.boolean("enabled", true).unwrap());

        for value in [false, true] {
            let parameters =
                FeatureParameters::from(HashMap::from([("enabled".into(), Value::Bool(value))]));

            assert_eq!(parameters.boolean("enabled", !value).unwrap(), value);
        }
    }

    #[test]
    fn malformed_parameters_report_the_option_without_using_the_default() {
        for value in [Value::Null, Value::from(1), Value::String("true".into())] {
            let parameters = FeatureParameters::from(HashMap::from([("enabled".into(), value)]));

            assert_eq!(
                parameters.boolean("enabled", true),
                Err(FeatureOptionsError::InvalidType {
                    name: "enabled".into(),
                    expected: "a boolean",
                })
            );
        }
    }
}
