//! Binary smoke tests. Each test runs in an isolated scratch dir. Environment
//! secrets are resolved lazily, so these offline suites need no real
//! credentials; the scratch dir still gets an empty `.env` for dotenvy.

use std::path::PathBuf;
use std::process::Command;

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vantage_cli_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

#[test]
fn help_flag_exits_successfully() {
    let dir = scratch_dir("help");
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .arg("--help")
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--file"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_arguments_prints_help_and_fails() {
    let dir = scratch_dir("noargs");
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn conflicting_file_and_group_flags_fail() {
    let dir = scratch_dir("conflict");
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--file", "x.json", "--group", "g.yaml"])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn benchmark_without_file_fails() {
    let dir = scratch_dir("bench_nofile");
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .arg("--benchmark")
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--file"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_scaffolds_the_workspace() {
    let dir = scratch_dir("init");
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .arg("--init")
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    for sub in ["sandbox", "test-suite", "groups", "reports"] {
        assert!(dir.join(sub).is_dir(), "{sub} should be created");
    }
    assert!(dir.join("sandbox").join("example.json").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dry_run_resolves_a_suite_without_sending_requests() {
    let dir = scratch_dir("dry_run");
    std::fs::write(
        dir.join("s.json"),
        r#"{"url":"{{BASE_URL}}/x","method":"POST",
            "tests":[{"name":"t","payload":{},"expected_response":{}}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--file", "s.json", "--dry-run"])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dry run"), "{stdout}");
    assert!(stdout.contains("no requests sent"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn names_filter_runs_only_selected_tests_and_reports_unmatched() {
    let dir = scratch_dir("names");
    std::fs::write(
        dir.join("s.json"),
        r#"{"url":"{{BASE_URL}}/x","method":"POST","tests":[
            {"name":"keep_me","payload":{},"expected_response":{}},
            {"name":"drop_me","payload":{},"expected_response":{}}
        ]}"#,
    )
    .unwrap();

    // --dry-run keeps this offline; --names selects one real test plus one
    // that matches nothing.
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--file", "s.json", "--dry-run", "--names", "keep_me,ghost"])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("keep_me"),
        "selected test should run: {stdout}"
    );
    assert!(
        !stdout.contains("drop_me"),
        "unselected test must be filtered out: {stdout}"
    );
    assert!(
        stderr.contains("no test named 'ghost'"),
        "an unmatched name must be reported: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_config_runs_one_profile_per_entry() {
    let dir = scratch_dir("group_config");
    std::fs::write(
        dir.join("s.json"),
        r#"{"url":"{{BASE_URL}}/x","method":"POST",
            "tests":[{"name":"only_test","payload":{},"expected_response":{}}]}"#,
    )
    .unwrap();
    // Two profiles; --dry-run keeps everything offline and (being global)
    // overrides the benchmark profile, so both profiles render the suite.
    std::fs::write(
        dir.join("g.yaml"),
        "name: G\nconfig:\n  - \"--verbose\"\n  - \"--benchmark --metrics\"\nfiles:\n  - ./s.json\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--group", "g.yaml", "--dry-run"])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // One "Dry run:" header per profile => the group ran twice.
    let runs = stdout.matches("Dry run:").count();
    assert_eq!(runs, 2, "expected one dry-run render per profile: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "email")]
#[test]
fn email_feature_writes_an_eml_with_the_report_attached() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let dir = scratch_dir("email");
    // A tiny local HTTP server so the suite completes offline.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in listener.incoming().take(4).flatten() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let body = b"{\"ok\":true}";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body);
        }
    });

    std::fs::write(
        dir.join("s.json"),
        format!(
            r#"{{"url":"http://127.0.0.1:{port}/x","method":"POST",
                "tests":[{{"name":"t","payload":{{}},"expected_response":{{"ok":true}}}}]}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("g.yaml"), "name: MyGroup\nfiles:\n  - ./s.json\n").unwrap();

    let outbox = dir.join("outbox");
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--group", "g.yaml", "--email", "qa@example.com"])
        .env("SMTP_TRANSPORT", "file")
        .env("SMTP_FILE_DIR", &outbox)
        .env("SMTP_FROM", "no-reply@example.com")
        .env("SMTP_HOST", "localhost")
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let emls: Vec<_> = std::fs::read_dir(&outbox)
        .expect("outbox should exist")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(emls.len(), 1, "exactly one email should be written");
    let eml = std::fs::read_to_string(emls[0].path()).unwrap();
    assert!(
        eml.contains("Subject: MyGroup"),
        "subject is the group name: {eml}"
    );
    assert!(eml.contains("qa@example.com"), "recipient present");
    assert!(eml.contains("index.html"), "HTML report attached");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_scaffolds_under_output_dir() {
    let dir = scratch_dir("init_out");
    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--init", "-o", "workspace"])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    for sub in ["sandbox", "test-suite", "groups", "reports"] {
        assert!(
            dir.join("workspace").join(sub).is_dir(),
            "{sub} should be created under the output dir"
        );
    }
    assert!(
        dir.join("workspace")
            .join("sandbox")
            .join("example.json")
            .is_file()
    );
    // The project root itself should stay clean.
    assert!(
        !dir.join("sandbox").exists(),
        "must not scaffold at the root"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn environments_file_override_replaces_the_embedded_config() {
    let dir = scratch_dir("env_override");
    // A runtime environments file whose `sandbox` env points at a distinctive
    // host and declares no secrets. Reusing the `sandbox` name keeps the
    // build-time argument validation happy.
    let envs = dir.join("envs.yaml");
    std::fs::write(
        &envs,
        "environments:\n  sandbox:\n    BASE_URL: https://override.example.com\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("s.json"),
        r#"{"url":"{{BASE_URL}}/x","method":"POST",
            "tests":[{"name":"t","payload":{},"expected_response":{}}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--file", "s.json", "--dry-run"])
        .env("VANTAGE_ENVIRONMENTS_FILE", &envs)
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The override host reaches the resolved request, proving the on-disk file
    // was used instead of the embedded config.
    assert!(
        stdout.contains("https://override.example.com/x"),
        "override BASE_URL should appear in the resolved request: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn environments_file_override_allows_new_environment_names() {
    let dir = scratch_dir("env_override_name");
    // `qa` is not a build-time environment name; with the override active the
    // `-e` validation must accept it and resolve it from the file.
    let envs = dir.join("envs.yaml");
    std::fs::write(
        &envs,
        "environments:\n  qa:\n    BASE_URL: https://qa.example.com\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("s.json"),
        r#"{"url":"{{BASE_URL}}/x","method":"POST",
            "tests":[{"name":"t","payload":{},"expected_response":{}}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cli"))
        .args(["--file", "s.json", "--dry-run", "-e", "qa"])
        .env("VANTAGE_ENVIRONMENTS_FILE", &envs)
        .current_dir(&dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("https://qa.example.com/x"),
        "the new env name should resolve from the override file: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
