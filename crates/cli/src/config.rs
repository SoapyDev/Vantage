//! Environment configuration: embedded at build time, overridable at runtime.
//!
//! `environments.yaml` is validated at build time (see `build.rs`) and embedded
//! into the binary here via `include_str!`. The set of valid environment names
//! is generated into [`ENVIRONMENT_NAMES`] and used to validate the
//! `--environment` / `--compare-with` arguments against the names known at
//! build time.
//!
//! Setting the `VANTAGE_ENVIRONMENTS_FILE` environment variable points [`load`]
//! at a YAML file on disk instead of the embedded copy, so a different set of
//! environments can be used without rebuilding. An override that keeps the
//! build-time environment names (e.g. `sandbox`) passes the argument validation
//! unchanged; introducing brand-new names would additionally require relaxing
//! [`validate_env_name`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use vantage_core::environments::EnvironmentsConfig;

/// Environment variable that, when set, points [`load`] at an on-disk
/// environments file instead of the build-time embedded copy.
const ENVIRONMENTS_FILE_VAR: &str = "VANTAGE_ENVIRONMENTS_FILE";

// Brings the generated `ENVIRONMENT_NAMES` constant into scope.
include!(concat!(env!("OUT_DIR"), "/environments_generated.rs"));

/// The raw `environments.yaml`, embedded into the binary at compile time.
const EMBEDDED_ENVIRONMENTS: &str = include_str!("../../../environments.yaml");

/// Loads the environment definitions.
///
/// If `VANTAGE_ENVIRONMENTS_FILE` is set, the file it points at is read and
/// parsed; otherwise the embedded build-time copy is used.
///
/// # Errors
///
/// Returns an error if an override file cannot be read, or if the chosen YAML
/// (override or embedded) cannot be parsed. For the embedded copy a parse error
/// should not happen in a successful build, since `build.rs` validates it.
pub fn load() -> Result<EnvironmentsConfig> {
    if let Some(path) = std::env::var_os(ENVIRONMENTS_FILE_VAR) {
        let path = PathBuf::from(path);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "failed to read {ENVIRONMENTS_FILE_VAR} at {}",
                path.display()
            )
        })?;
        return serde_yaml_ng::from_str(&text)
            .with_context(|| format!("failed to parse environments file {}", path.display()));
    }

    serde_yaml_ng::from_str(EMBEDDED_ENVIRONMENTS)
        .context("failed to parse embedded environments.yaml")
}

/// clap value parser for environment names.
///
/// By default a name is checked against the environments known at build time.
/// When `VANTAGE_ENVIRONMENTS_FILE` is set the valid names come from that file
/// instead, which is not available at parse time, so the check is relaxed to
/// only reject an empty name; an unknown name is then caught at load/resolve
/// time with the available list from the override file.
///
/// # Errors
///
/// Without an override, returns a human-readable error listing the available
/// names when `value` is not a defined environment. With an override, returns
/// an error only when `value` is empty.
pub fn validate_env_name(value: &str) -> Result<String, String> {
    if std::env::var_os(ENVIRONMENTS_FILE_VAR).is_some() {
        return if value.is_empty() {
            Err("environment name must not be empty".to_string())
        } else {
            Ok(value.to_string())
        };
    }

    if ENVIRONMENT_NAMES.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(format!(
            "unknown environment '{value}' (available: {})",
            ENVIRONMENT_NAMES.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_config_parses_and_contains_sandbox() {
        let config = load().unwrap();
        assert!(config.contains("sandbox"));
    }

    #[test]
    fn generated_names_include_sandbox() {
        // Non-emptiness is already a build-time guarantee (build.rs fails
        // the build on an empty environments.yaml).
        assert!(ENVIRONMENT_NAMES.contains(&"sandbox"));
    }

    #[test]
    fn validate_accepts_known_and_rejects_unknown() {
        assert_eq!(validate_env_name("sandbox").unwrap(), "sandbox");
        assert!(validate_env_name("does-not-exist").is_err());
    }
}
