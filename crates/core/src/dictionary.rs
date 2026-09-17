use crate::environments::{ConfigError, EnvironmentsConfig};
use serde_json::Value;
use std::collections::HashMap;

/// Template variables made available to a test suite.
///
/// `variables` drives the primary environment; `compare_variables` holds the
/// second environment used by compare mode (the left-/right-hand sides of a
/// comparison run). In a normal (non-compare) run `compare_variables` stays
/// empty.
#[derive(Default, Clone, Debug)]
pub struct Dictionary {
    pub variables: HashMap<String, Value>,
    pub compare_variables: HashMap<String, Value>,
}

impl Dictionary {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a dictionary from an environments configuration.
    ///
    /// `environment` populates [`variables`](Self::variables); when
    /// `compare_environment` is provided, it populates
    /// [`compare_variables`](Self::compare_variables) so compare mode can issue
    /// both sides of a comparison. `env_vars` supplies the values for any
    /// `${ENV_KEY}` placeholders (typically `std::env::vars()` after loading
    /// `.env`).
    ///
    /// # Errors
    ///
    /// Propagates [`ConfigError`] when an environment name is unknown or a
    /// referenced environment variable is missing.
    pub fn from_config(
        config: &EnvironmentsConfig,
        environment: &str,
        compare_environment: Option<&str>,
        env_vars: &HashMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let mut dictionary = Self::new();
        dictionary.variables = config.resolve(environment, env_vars)?;

        if let Some(compare) = compare_environment {
            dictionary.compare_variables = config.resolve(compare, env_vars)?;
        }

        Ok(dictionary)
    }

    /// Inserts (or replaces) a primary-environment variable.
    pub fn insert(&mut self, key: String, value: Value) {
        self.variables.insert(key, value);
    }

    /// Looks up a primary-environment variable.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.variables.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EnvironmentsConfig {
        let mut primary = HashMap::new();
        primary.insert("BASE_URL".to_string(), "https://primary".to_string());
        primary.insert("CLIENT_ID".to_string(), "${ID}".to_string());

        let mut secondary = HashMap::new();
        secondary.insert("BASE_URL".to_string(), "https://secondary".to_string());

        let mut environments = HashMap::new();
        environments.insert("primary".to_string(), primary);
        environments.insert("secondary".to_string(), secondary);
        EnvironmentsConfig { environments }
    }

    fn env_vars() -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("ID".to_string(), "client-1".to_string());
        map
    }

    #[test]
    fn from_config_populates_only_primary_without_compare() {
        let dict = Dictionary::from_config(&config(), "primary", None, &env_vars()).unwrap();
        assert_eq!(
            dict.get("BASE_URL").and_then(Value::as_str),
            Some("https://primary")
        );
        assert_eq!(
            dict.variables.get("CLIENT_ID").and_then(Value::as_str),
            Some("client-1")
        );
        assert!(dict.compare_variables.is_empty());
    }

    #[test]
    fn from_config_populates_both_sides_for_compare() {
        let dict =
            Dictionary::from_config(&config(), "primary", Some("secondary"), &env_vars()).unwrap();
        assert_eq!(
            dict.variables.get("BASE_URL").and_then(Value::as_str),
            Some("https://primary")
        );
        assert_eq!(
            dict.compare_variables
                .get("BASE_URL")
                .and_then(Value::as_str),
            Some("https://secondary")
        );
    }

    #[test]
    fn from_config_surfaces_unknown_environment() {
        let err = Dictionary::from_config(&config(), "ghost", None, &env_vars()).unwrap_err();
        assert!(matches!(err, ConfigError::UnknownEnvironment { .. }));
    }
}
