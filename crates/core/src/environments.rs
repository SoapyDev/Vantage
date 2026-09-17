//! Environment definitions loaded from a configuration file.
//!
//! An [`EnvironmentsConfig`] maps an environment name (e.g. `sandbox`,
//! `staging`) to a flat set of template variables. Variable values may
//! embed `${ENV_KEY}` placeholders that are resolved against the process
//! environment (typically populated from a `.env` file) by [`resolve`].
//!
//! This module is deliberately free of any file or HTTP I/O: it operates on an
//! already-deserialized config and a caller-supplied environment-variable map,
//! so the crate stays I/O-free and the resolution logic stays unit-testable.
//!
//! [`resolve`]: EnvironmentsConfig::resolve

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

/// The parsed contents of an environments configuration file.
///
/// Each entry maps an environment name to its template variables. The variable
/// values are stored verbatim (including any `${ENV_KEY}` placeholders); they
/// are only resolved when [`resolve`](Self::resolve) is called.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct EnvironmentsConfig {
    /// Environment name -> (variable name -> raw value).
    pub environments: HashMap<String, HashMap<String, String>>,
}

/// An error produced while resolving an environment into dictionary variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The requested environment name is not defined in the configuration.
    UnknownEnvironment {
        /// The name that was requested.
        name: String,
        /// The names that are available, sorted for a stable message.
        available: Vec<String>,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownEnvironment { name, available } => write!(
                f,
                "unknown environment '{name}' (available: {})",
                available.join(", ")
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl EnvironmentsConfig {
    /// Returns the configured environment names, sorted alphabetically.
    #[must_use]
    pub fn sorted_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.environments.keys().cloned().collect();
        names.sort();
        names
    }

    /// Returns `true` if `name` is a defined environment.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.environments.contains_key(name)
    }

    /// Resolves a single environment into a set of dictionary variables,
    /// substituting every `${ENV_KEY}` placeholder with the matching value from
    /// `env_vars`.
    ///
    /// Resolution is lazy with respect to secrets: a variable whose
    /// `${ENV_KEY}` placeholder is not set in `env_vars` is simply omitted
    /// rather than failing the whole run. An environment typically declares
    /// more variables (client id, secret, tenant) than any single suite uses,
    /// so an unused, unset secret should not stop startup. A variable that is
    /// actually referenced by a request but ends up missing surfaces later as
    /// an unresolved-variable error, with the variables in scope listed.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownEnvironment`] if `name` is not defined.
    pub fn resolve(
        &self,
        name: &str,
        env_vars: &HashMap<String, String>,
    ) -> Result<HashMap<String, Value>, ConfigError> {
        let variables =
            self.environments
                .get(name)
                .ok_or_else(|| ConfigError::UnknownEnvironment {
                    name: name.to_string(),
                    available: self.sorted_names(),
                })?;

        let mut resolved = HashMap::with_capacity(variables.len());
        for (key, raw) in variables {
            // Skip variables with unset placeholders (see the lazy-resolution
            // note above); only fully-substituted values enter the dictionary.
            if let Ok(value) = substitute(raw, env_vars) {
                resolved.insert(key.clone(), Value::String(value));
            }
        }
        Ok(resolved)
    }
}

/// Replaces every `${IDENT}` placeholder in `raw` with the corresponding value
/// from `env_vars`. Returns the missing key name as the error when a referenced
/// variable is absent.
fn substitute(raw: &str, env_vars: &HashMap<String, String>) -> Result<String, String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)}").expect("placeholder regex is valid")
    });

    let mut out = String::with_capacity(raw.len());
    let mut last_end = 0usize;

    for caps in re.captures_iter(raw) {
        // capture group 0 always exists within an iteration.
        let whole = caps.get(0).expect("match 0 always present");
        out.push_str(&raw[last_end..whole.start()]);

        let key = caps
            .get(1)
            .expect("capture group 1 always present")
            .as_str();
        let value = env_vars.get(key).ok_or_else(|| key.to_string())?;
        out.push_str(value);

        last_end = whole.end();
    }

    out.push_str(&raw[last_end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EnvironmentsConfig {
        let mut sandbox = HashMap::new();
        sandbox.insert(
            "BASE_URL".to_string(),
            "https://sandbox.example.com".to_string(),
        );
        sandbox.insert("CLIENT_ID".to_string(), "${AUTH_CLIENT_ID}".to_string());
        sandbox.insert("TENANT_ID".to_string(), String::new());

        let mut staging = HashMap::new();
        staging.insert(
            "BASE_URL".to_string(),
            "https://staging.example.com".to_string(),
        );
        staging.insert("CLIENT_ID".to_string(), "${STAGING_CLIENT_ID}".to_string());

        let mut environments = HashMap::new();
        environments.insert("sandbox".to_string(), sandbox);
        environments.insert("staging".to_string(), staging);
        EnvironmentsConfig { environments }
    }

    fn env_vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn resolves_placeholders_and_literals() {
        let vars = env_vars(&[("AUTH_CLIENT_ID", "the-id")]);
        let resolved = config().resolve("sandbox", &vars).unwrap();

        assert_eq!(
            resolved.get("BASE_URL").and_then(Value::as_str),
            Some("https://sandbox.example.com")
        );
        assert_eq!(
            resolved.get("CLIENT_ID").and_then(Value::as_str),
            Some("the-id")
        );
        assert_eq!(resolved.get("TENANT_ID").and_then(Value::as_str), Some(""));
    }

    #[test]
    fn unknown_environment_is_an_error_listing_available() {
        let err = config().resolve("nope", &HashMap::new()).unwrap_err();
        assert_eq!(
            err,
            ConfigError::UnknownEnvironment {
                name: "nope".to_string(),
                available: vec!["sandbox".to_string(), "staging".to_string()],
            }
        );
        assert!(err.to_string().contains("sandbox"));
    }

    #[test]
    fn unset_placeholders_are_skipped_not_errors() {
        // `sandbox` declares CLIENT_ID via `${AUTH_CLIENT_ID}`; with no env
        // vars supplied it is omitted rather than failing, while literal
        // variables (TENANT_ID = "") still resolve.
        let resolved = config().resolve("sandbox", &HashMap::new()).unwrap();
        assert!(!resolved.contains_key("CLIENT_ID"));
        assert_eq!(resolved.get("TENANT_ID").and_then(Value::as_str), Some(""));
    }

    #[test]
    fn substitution_supports_embedded_placeholders() {
        let vars = env_vars(&[("TENANT", "abc")]);
        let out = substitute("https://host/{tenant}/${TENANT}/token", &vars).unwrap();
        assert_eq!(out, "https://host/{tenant}/abc/token");
    }

    #[test]
    fn substitution_without_placeholders_is_identity() {
        assert_eq!(substitute("plain", &HashMap::new()).unwrap(), "plain");
    }

    #[test]
    fn contains_and_sorted_names() {
        let cfg = config();
        assert!(cfg.contains("sandbox"));
        assert!(!cfg.contains("missing"));
        assert_eq!(cfg.sorted_names(), vec!["sandbox", "staging"]);
    }
}
