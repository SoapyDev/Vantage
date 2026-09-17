use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::groups::Group;
use anyhow::{Context, Result, anyhow};
use vantage_core::test_suite::TestSuite;

pub fn load_test_suite(path: &Path) -> Result<TestSuite> {
    let file =
        File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
    let reader = BufReader::new(file);

    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    let mut suite: TestSuite = match extension {
        "json" => serde_json::from_reader(reader)
            .context(format!("Failed to parse JSON: {}", path.display()))?,
        _ => anyhow::bail!("Unsupported file format: {}", path.display()),
    };

    suite.file_path = Some(path.to_string_lossy().to_string());
    Ok(suite)
}

pub fn load_group(path: &Path) -> Result<Group> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open group file: {}", path.display()))?;
    let reader = BufReader::new(file);

    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    let mut group: Group = match extension {
        "yaml" | "yml" => serde_yaml_ng::from_reader(reader)
            .context(format!("Failed to parse YAML: {}", path.display()))?,
        _ => anyhow::bail!("Unsupported file format: {}", path.display()),
    };

    group.source_path = path.to_string_lossy().to_string();
    Ok(group)
}

/// Collects the `.json` test suites of `path`, sorted by name.
///
/// Only `.json` is collected: [`load_test_suite`] supports nothing else, so
/// collecting other extensions here would only fail later, with a worse error.
pub fn scan_directory(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file()
                && let Some(ext) = path.extension()
                && ext == "json"
            {
                files.push(path);
            }
        }
    }

    if files.is_empty() {
        return Err(anyhow!(
            "No test suite files (.json) found in {}",
            path.display()
        ));
    }

    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/files")
            .join(name)
    }

    // ---------- load_test_suite ----------

    #[test]
    fn loads_a_valid_json_suite_and_records_its_path() {
        let path = fixture("valid_suite.json");
        let suite = load_test_suite(&path).unwrap();
        assert_eq!(suite.tests.len(), 1);
        assert!(
            suite
                .file_path
                .as_deref()
                .unwrap_or("")
                .ends_with("valid_suite.json")
        );
    }

    #[test]
    fn missing_suite_file_is_an_error() {
        let err = load_test_suite(&fixture("does-not-exist.json")).unwrap_err();
        assert!(err.to_string().contains("Failed to open"), "{err}");
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let err = load_test_suite(&fixture("malformed.json")).unwrap_err();
        assert!(err.to_string().contains("Failed to parse JSON"), "{err}");
    }

    #[test]
    fn unsupported_extension_is_rejected() {
        let err = load_test_suite(&fixture("unsupported_format.toml")).unwrap_err();
        assert!(err.to_string().contains("Unsupported file format"), "{err}");
    }

    // ---------- load_group ----------

    #[test]
    fn loads_a_valid_yaml_group() {
        let path = fixture("group.yaml");
        let group = load_group(&path).unwrap();
        assert_eq!(group.name, "Test group");
        assert_eq!(group.files.len(), 1);
        assert!(group.source_path.ends_with("group.yaml"));
    }

    #[test]
    fn group_with_unsupported_extension_is_rejected() {
        let err = load_group(&fixture("valid_suite.json")).unwrap_err();
        assert!(err.to_string().contains("Unsupported file format"), "{err}");
    }

    #[test]
    fn malformed_group_yaml_is_a_parse_error() {
        let err = load_group(&fixture("malformed.yaml")).unwrap_err();
        assert!(err.to_string().contains("Failed to parse YAML"), "{err}");
    }

    // ---------- scan_directory ----------

    #[test]
    fn scan_directory_returns_sorted_json_suites_only() {
        let dir = std::env::temp_dir().join(format!("vantage_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.json"), "{}").unwrap();
        std::fs::write(dir.join("a.yaml"), "x: 1").unwrap();
        std::fs::write(dir.join("ignored.txt"), "nope").unwrap();

        let files = scan_directory(&dir).unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        // Only .json is scanned: load_test_suite supports nothing else, so
        // collecting YAML here would only fail later with a worse error.
        assert_eq!(names, vec!["b.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_directory_with_no_suite_files_is_an_error() {
        let dir = std::env::temp_dir().join(format!("vantage_scan_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(scan_directory(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
