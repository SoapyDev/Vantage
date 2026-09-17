//! Workspace scaffolding for `--init` and `--sandbox`: creates the standard
//! directory layout and writes an annotated `sandbox/example.json` so a fresh
//! checkout has something runnable. All operations are non-destructive --
//! existing files and directories are left untouched.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Annotated example suite written by `--init` and by `--sandbox` on first run.
/// It exercises the full grammar (a `steps` setup that captures an OAuth token,
/// then an asserted `tests` request that uses it) so it doubles as a template.
const EXAMPLE_TEST: &str = r#"{
  "$schema": "../schema/suite.schema.json",
  "_note": "Example suite from `cli --init`. Edit url/payload/expected_response, then run `cli -f ./sandbox/example.json`. Full syntax: test-suite/README.md.",
  "url": "{{BASE_URL}}/api/services/MyService/MyEndpoint",
  "method": "POST",
  "ignored_fields": ["$id"],
  "steps": [
    {
      "_note": "Setup step (not asserted): fetch an OAuth token and capture it for the tests below.",
      "name": "Get auth token",
      "url": "{{IDENTITY_SERVER_BASE_URL}}/{{TENANT_ID}}/oauth2/v2.0/token",
      "content_type": "application/x-www-form-urlencoded",
      "payload": {
        "client_id": "{{CLIENT_ID}}",
        "client_secret": "{{CLIENT_SECRET}}",
        "grant_type": "client_credentials",
        "scope": "{{BASE_URL}}/.default"
      },
      "capture": { "access_token": "/access_token" }
    }
  ],
  "tests": [
    {
      "_note": "Asserted request: the response is compared against expected_response (after ignored_fields/sorts).",
      "name": "Example test",
      "headers": { "Authorization": "Bearer {{access_token}}" },
      "payload": {},
      "expected_response": {}
    }
  ]
}
"#;

/// Standard workspace directories created by `--init` when missing.
const WORKSPACE_DIRS: &[&str] = &["sandbox", "test-suite", "groups", "reports"];

/// Ensures `<base>/sandbox` exists, seeding it with [`EXAMPLE_TEST`] on
/// creation. `base` is the resolved output directory, so `--sandbox` looks
/// where `--init` scaffolds.
///
/// # Errors
///
/// Returns an error if the directory or the example file cannot be created.
pub fn ensure_sandbox_exists(base: &Path) -> Result<PathBuf> {
    let path = base.join("sandbox");

    if !path.exists() {
        println!("Creating Sandbox directory");
        std::fs::create_dir_all(&path).context("Failed to create sandbox directory")?;

        let example_path = path.join("example.json");
        let mut file = File::create(&example_path).context("Failed to create example file")?;
        file.write_all(EXAMPLE_TEST.as_bytes())?;
    }
    Ok(path)
}

/// Scaffolds the workspace under `root`: creates the standard directories when
/// missing and writes an annotated `sandbox/example.json` when absent.
/// Non-destructive -- existing files and directories are left untouched.
/// Returns the paths it created (relative to `root`), for reporting.
///
/// # Errors
///
/// Returns an error if a directory or the example file cannot be created.
pub fn init_workspace_in(root: &Path) -> Result<Vec<String>> {
    let mut created = Vec::new();

    for dir in WORKSPACE_DIRS {
        let path = root.join(dir);
        if !path.exists() {
            std::fs::create_dir_all(&path)
                .with_context(|| format!("failed to create directory '{}'", path.display()))?;
            created.push(format!("{dir}/"));
        }
    }

    let example = root.join("sandbox").join("example.json");
    if !example.exists() {
        std::fs::write(&example, EXAMPLE_TEST)
            .with_context(|| format!("failed to write '{}'", example.display()))?;
        created.push("sandbox/example.json".to_string());
    }

    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vantage_core::test_suite::TestSuite;

    #[test]
    fn embedded_example_is_a_valid_suite_with_valid_pointers() {
        let suite: TestSuite = serde_json::from_str(EXAMPLE_TEST).expect("example must parse");
        for request in suite.steps.iter().chain(suite.tests.iter()) {
            for pointer in request.capture.values() {
                assert!(
                    pointer.is_empty() || pointer.starts_with('/'),
                    "capture pointer '{pointer}' is not a valid JSON pointer"
                );
            }
        }
    }

    #[test]
    fn ensure_sandbox_exists_uses_the_given_base() {
        let root = std::env::temp_dir().join(format!("vantage_sb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let sandbox = ensure_sandbox_exists(&root).unwrap();
        assert_eq!(sandbox, root.join("sandbox"));
        assert!(root.join("sandbox").join("example.json").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn init_workspace_scaffolds_the_layout_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!("vantage_init_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let created = init_workspace_in(&root).unwrap();
        for dir in ["sandbox", "test-suite", "groups", "reports"] {
            assert!(root.join(dir).is_dir(), "{dir} should be created");
        }
        let example = root.join("sandbox").join("example.json");
        assert!(example.is_file(), "example.json should be written");
        assert!(created.iter().any(|p| p == "sandbox/example.json"));

        // The generated example parses as a suite.
        let text = std::fs::read_to_string(&example).unwrap();
        let _suite: TestSuite = serde_json::from_str(&text).expect("example must parse");

        // Running again creates nothing.
        let again = init_workspace_in(&root).unwrap();
        assert!(again.is_empty(), "init must be idempotent: {again:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
