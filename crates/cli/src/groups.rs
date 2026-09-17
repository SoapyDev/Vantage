use serde::Deserialize;

/// A group file (`groups/*.yml`): a named list of suite files to run
/// together, with optional run configuration. Loaded by
/// [`crate::loader::load_group`], which also records `source_path`.
#[derive(Debug, Clone, Deserialize)]
pub struct Group {
    pub name: String,
    pub files: Vec<String>,

    /// Optional run configuration: one or more flag strings such as
    /// `"--reports"` or `"--benchmark --metrics"`. Each string is a *profile*
    /// the group runs over all of its files (with the command line merged on
    /// top). Absent means a single run driven entirely by the command line.
    #[serde(default)]
    pub config: Option<GroupConfig>,

    #[serde(default)]
    pub source_path: String,
}

/// The `config:` value of a [`Group`]: either a single flag string or a list
/// of them. Deserialized untagged so both YAML shapes work:
///
/// ```yaml
/// config: "--reports"
/// # or
/// config:
///   - "--reports"
///   - "--benchmark --metrics"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GroupConfig {
    /// A single profile.
    One(String),
    /// An ordered list of profiles.
    Many(Vec<String>),
}

impl Group {
    /// The run profiles declared by the group, in order. Empty when no
    /// `config` was provided, in which case the caller runs the group once
    /// from the command-line arguments.
    #[must_use]
    pub fn profiles(&self) -> Vec<String> {
        match &self.config {
            None => Vec::new(),
            Some(GroupConfig::One(profile)) => vec![profile.clone()],
            Some(GroupConfig::Many(profiles)) => profiles.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Group {
        serde_yaml_ng::from_str(yaml).expect("valid group yaml")
    }

    #[test]
    fn config_is_optional_and_yields_no_profiles() {
        let group = parse("name: G\nfiles:\n  - ./a.json\n");
        assert!(group.profiles().is_empty());
    }

    #[test]
    fn a_single_config_string_is_one_profile() {
        let group = parse("name: G\nconfig: \"--reports\"\nfiles:\n  - ./a.json\n");
        assert_eq!(group.profiles(), vec!["--reports".to_string()]);
    }

    #[test]
    fn a_config_list_keeps_order() {
        let group = parse(
            "name: G\nconfig:\n  - \"--reports\"\n  - \"--benchmark --metrics\"\nfiles:\n  - ./a.json\n",
        );
        assert_eq!(
            group.profiles(),
            vec!["--reports".to_string(), "--benchmark --metrics".to_string()]
        );
    }
}
