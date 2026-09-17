//! Integration tests for the open-loop `--load` scheduler against a local mock.
//!
//! Timing-based, so bounds are deliberately loose: they assert the scheduler's
//! *behaviour* (rate roughly tracks the curve, the in-flight cap throttles, the
//! adaptive stop trips and can be disabled) rather than exact counts.

use std::time::Duration;

use mock_server::{MockResponse, MockServer};
use runner::benchmark::{BenchConfig, run_benchmark};
use serde_json::{Value, json};
use vantage_core::benchmark::BenchReport;
use vantage_core::dictionary::Dictionary;
use vantage_core::load::LoadProfile;
use vantage_core::test_suite::TestSuite;

/// A mock with an instant `/ok`, a slow `/slow` (200 ms), and a `/limited`
/// that always answers 429.
async fn spawn_mock() -> MockServer {
    MockServer::builder()
        .route("/ok", MockResponse::json(200, json!({"ok": true})))
        .route(
            "/slow",
            MockResponse::json(200, json!({"ok": true})).with_delay(Duration::from_millis(200)),
        )
        .route(
            "/limited",
            MockResponse::json(429, json!({"error": "too many"})),
        )
        .spawn()
        .await
}

fn suite_hitting(path: &str, tests: usize) -> TestSuite {
    let tests: Vec<Value> = (0..tests)
        .map(|i| json!({"name": format!("t{i}"), "payload": {}}))
        .collect();
    serde_json::from_value(json!({
        "url": format!("{{{{BASE_URL}}}}{path}"),
        "method": "POST",
        "steps": [],
        "tests": tests,
    }))
    .unwrap()
}

fn dictionary_for(base: &str) -> Dictionary {
    let mut dictionary = Dictionary::new();
    dictionary.insert("BASE_URL".to_string(), json!(base));
    dictionary
}

/// Runs a benchmark whose only measured part is the load profile unless
/// `escalation` is set.
async fn run_load(
    server: &MockServer,
    path: &str,
    tests: usize,
    spec: &str,
    escalation: bool,
    max_in_flight: usize,
    adaptive_stop: bool,
) -> BenchReport {
    let mut suite = suite_hitting(path, tests);
    let mut dictionary = dictionary_for(server.base_url());
    run_benchmark(
        &reqwest::Client::new(),
        &mut suite,
        &mut dictionary,
        BenchConfig {
            pool_size: 8,
            max_concurrency: Some(1),
            seed: 42,
            load: Some(LoadProfile::parse(spec).unwrap()),
            escalation,
            max_in_flight,
            adaptive_stop,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn load_profile_runs_alongside_escalation_and_tracks_the_rate() {
    let server = spawn_mock().await;
    // 10 CPS for 2 s -> ~20 dispatches; escalation also runs first.
    let report = run_load(&server, "/ok", 4, "step:2s:10", true, 256, true).await;

    let load = report.load.expect("a load report must be present");
    let fired = load.samples.len();
    assert!(
        (10..=30).contains(&fired),
        "≈20 requests expected at 10 CPS over 2 s, got {fired}"
    );
    assert!(
        load.samples.iter().all(|s| s.is_success),
        "every /ok call succeeds"
    );
    assert!(
        load.samples.iter().any(|s| s.offset_ms >= 1000),
        "dispatch offsets must span the profile"
    );
    assert_eq!(load.stages.len(), 1, "the run's stages are echoed");
    assert!(load.buckets.len() >= 2, "per-second buckets cover the run");
    assert!(
        load.stop_reason.is_none(),
        "a healthy run does not stop early"
    );
    assert!(!report.phases.is_empty(), "escalation still ran up front");
}

#[tokio::test]
async fn load_profile_runs_alone_when_escalation_is_disabled() {
    let server = spawn_mock().await;
    let report = run_load(&server, "/ok", 4, "step:1s:5", false, 256, true).await;

    assert!(
        report.phases.is_empty(),
        "no escalation phases when escalation is off"
    );
    assert!(report.load.is_some(), "the load profile still ran");
}

#[tokio::test]
async fn in_flight_cap_throttles_dispatch() {
    let server = spawn_mock().await;
    // Ask for 100 CPS against a 200 ms endpoint but allow only 2 in flight:
    // the cap, not the target, governs how many calls actually go out.
    let report = run_load(&server, "/slow", 8, "step:1s:100", false, 2, false).await;

    let load = report.load.unwrap();
    let fired = load.samples.len();
    assert!(
        fired <= 30,
        "the in-flight cap must hold dispatch far below the 100/s target, got {fired}"
    );
    assert!(fired >= 1, "some requests still go out");
}

#[tokio::test]
async fn adaptive_stop_trips_on_rate_limiting() {
    let server = spawn_mock().await;
    let report = run_load(&server, "/limited", 8, "step:3s:20", false, 256, true).await;

    let load = report.load.unwrap();
    let reason = load
        .stop_reason
        .expect("a 429 storm must trip the adaptive stop");
    assert!(reason.contains("429"), "{reason}");
    assert!(
        load.samples.len() < 60,
        "the run must stop before the full 3 s * 20 CPS budget"
    );
}

#[tokio::test]
async fn adaptive_stop_can_be_disabled() {
    let server = spawn_mock().await;
    let report = run_load(&server, "/limited", 8, "step:1s:15", false, 256, false).await;

    let load = report.load.unwrap();
    assert!(
        load.stop_reason.is_none(),
        "with the adaptive stop off, a 429 storm does not halt the run"
    );
    assert!(
        load.samples.len() >= 8,
        "the whole profile ran despite the errors"
    );
}
