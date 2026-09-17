//! Integration tests for the --benchmark protocol against a local mock.

use runner::benchmark::{BenchConfig, run_benchmark};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use vantage_core::benchmark::BenchReport;
use vantage_core::dictionary::Dictionary;
use vantage_core::test_suite::TestSuite;

/// Mock server: `/ok` -> 200, `/limited` -> 429, `/flaky` -> 500.
async fn spawn_mock_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                        break pos + 4;
                    }
                };

                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let content_length = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);

                while buf.len() < header_end + content_length {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                }

                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();

                let (status, body): (u16, Value) = match path.as_str() {
                    "/ok" => (200, json!({"ok": true})),
                    "/limited" => (429, json!({"error": "too many requests"})),
                    "/flaky" => (500, json!({"error": "boom"})),
                    _ => (404, json!({"error": "not found"})),
                };

                let bytes = serde_json::to_vec(&body).unwrap();
                let head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nServer-Timing: total;dur=5,db;dur=3\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&bytes).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    format!("http://{addr}")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn suite_with(tests: Vec<Value>) -> TestSuite {
    serde_json::from_value(json!({
        "url": "{{BASE_URL}}/ok",
        "method": "POST",
        "steps": [],
        "tests": tests
    }))
    .unwrap()
}

fn ok_test(name: &str) -> Value {
    json!({"name": name, "payload": {}})
}

fn dictionary_for(base: &str) -> Dictionary {
    let mut dictionary = Dictionary::new();
    dictionary.insert("BASE_URL".to_string(), json!(base));
    dictionary
}

async fn bench(
    suite: &mut TestSuite,
    dictionary: &mut Dictionary,
    pool_size: usize,
    max_concurrency: Option<usize>,
) -> BenchReport {
    run_benchmark(
        &reqwest::Client::new(),
        suite,
        dictionary,
        BenchConfig {
            pool_size,
            max_concurrency,
            seed: 42,
            load: None,
            escalation: true,
            max_in_flight: runner::benchmark::DEFAULT_MAX_IN_FLIGHT,
            adaptive_stop: true,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn samples_carry_the_server_timing_when_the_service_reports_it() {
    let base = spawn_mock_server().await;
    let tests: Vec<Value> = (0..4).map(|i| ok_test(&format!("t{i}"))).collect();
    let mut suite = suite_with(tests);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 4, Some(1)).await;

    // The mock reports `total;dur=5,db;dur=3` on every response.
    for sample in report.phases.iter().flat_map(|p| &p.samples) {
        assert_eq!(
            sample.server_ms,
            Some(5.0),
            "the total metric is the server-reported processing time"
        );
        assert_eq!(sample.server_parts.len(), 1, "db is a component");
        assert_eq!(sample.server_parts[0].name, "db");
        assert_eq!(sample.server_parts[0].dur_ms, 3.0);
    }
}

#[tokio::test]
async fn samples_carry_the_call_name() {
    let base = spawn_mock_server().await;
    let tests: Vec<Value> = (0..4).map(|i| ok_test(&format!("t{i}"))).collect();
    let mut suite = suite_with(tests);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 4, Some(1)).await;

    for sample in report.phases.iter().flat_map(|p| &p.samples) {
        assert!(
            sample.name.starts_with('t'),
            "each sample must carry its call name, got {:?}",
            sample.name
        );
    }
}

#[tokio::test]
async fn latency_probe_runs_before_the_phases() {
    let base = spawn_mock_server().await;
    let tests: Vec<Value> = (0..4).map(|i| ok_test(&format!("t{i}"))).collect();
    let mut suite = suite_with(tests);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 4, Some(2)).await;

    let latency = report.latency.expect("the latency probe must have run");
    assert_eq!(
        latency.tcp_us.len(),
        runner::benchmark::LATENCY_PROBES,
        "one TCP sample per probe against a local listener"
    );
    assert_eq!(
        latency.http_us.len(),
        runner::benchmark::LATENCY_PROBES,
        "one HTTP sample per probe against a local listener"
    );
    // Probes are not phases and must not pollute the measured stats.
    assert_eq!(
        report
            .phases
            .iter()
            .map(|p| p.concurrency)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn escalation_stops_when_a_level_exceeds_the_pool_size() {
    let base = spawn_mock_server().await;
    let tests: Vec<Value> = (0..6).map(|i| ok_test(&format!("t{i}"))).collect();
    let mut suite = suite_with(tests);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 4, None).await;

    assert_eq!(report.pool_size, 4);
    assert_eq!(report.warmup_count, 0);
    // x8 over a pool of 4 cannot create more parallelism than x4 did.
    assert_eq!(
        report
            .phases
            .iter()
            .map(|p| p.concurrency)
            .collect::<Vec<_>>(),
        vec![1, 2, 4],
        "levels above the pool size are pointless and must be skipped"
    );
    for phase in &report.phases {
        assert_eq!(phase.samples.len(), 4, "same pool at every level");
        assert_eq!(phase.error_count(), 0);
    }
    let reason = report.stop_reason.expect("a stop reason must explain why");
    assert!(reason.contains("pool"), "{reason}");
}

#[tokio::test]
async fn escalation_reaches_the_high_levels_with_a_large_pool() {
    let base = spawn_mock_server().await;
    let tests: Vec<Value> = (0..6).map(|i| ok_test(&format!("t{i}"))).collect();
    let mut suite = suite_with(tests);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 32, Some(32)).await;

    assert_eq!(
        report
            .phases
            .iter()
            .map(|p| p.concurrency)
            .collect::<Vec<_>>(),
        vec![1, 2, 4, 8, 16, 32],
        "the new levels must run when the pool and the cap allow them"
    );
    for phase in &report.phases {
        assert_eq!(phase.samples.len(), 32, "same pool at every level");
    }
    let reason = report.stop_reason.expect("x64 is beyond the cap");
    assert!(reason.contains("32"), "{reason}");
}

#[tokio::test]
async fn expected_failures_are_used_as_warmup_then_excluded() {
    let base = spawn_mock_server().await;
    let mut suite = suite_with(vec![
        ok_test("ok-1"),
        ok_test("ok-2"),
        json!({"name": "invalid", "url": "{{BASE_URL}}/flaky", "payload": {}, "expected_status": 500}),
    ]);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 16, None).await;

    assert_eq!(report.warmup_count, 1);
    // The 2 eligible tests are cycled up to the requested pool size; the
    // expected-failure test stays out (phases below must show 0 errors).
    assert_eq!(report.pool_size, 16);
    for phase in &report.phases {
        assert_eq!(
            phase.error_count(),
            0,
            "warm-up requests must not pollute stats"
        );
    }
}

#[tokio::test]
async fn rate_limiting_stops_after_the_first_phase() {
    let base = spawn_mock_server().await;
    let mut suite = suite_with(vec![
        json!({"name": "limited", "url": "{{BASE_URL}}/limited", "payload": {}}),
        json!({"name": "limited-2", "url": "{{BASE_URL}}/limited", "payload": {}}),
    ]);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 16, None).await;

    assert_eq!(report.phases.len(), 1, "no escalation after a 429");
    let reason = report.stop_reason.unwrap();
    assert!(reason.contains("429"), "{reason}");
}

#[tokio::test]
async fn high_error_rate_stops_the_escalation() {
    let base = spawn_mock_server().await;
    let mut suite = suite_with(vec![
        json!({"name": "broken", "url": "{{BASE_URL}}/flaky", "payload": {}}),
        ok_test("fine"),
    ]);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 16, None).await;

    // 50% d'erreurs en séquentiel > seuil de 20% -> arrêt immédiat
    assert_eq!(report.phases.len(), 1);
    let reason = report.stop_reason.unwrap();
    assert!(reason.contains('%'), "{reason}");
}

#[tokio::test]
async fn concurrency_cap_limits_the_levels() {
    let base = spawn_mock_server().await;
    let tests: Vec<Value> = (0..4).map(|i| ok_test(&format!("t{i}"))).collect();
    let mut suite = suite_with(tests);
    let mut dictionary = dictionary_for(&base);

    let report = bench(&mut suite, &mut dictionary, 8, Some(4)).await;

    assert_eq!(
        report
            .phases
            .iter()
            .map(|p| p.concurrency)
            .collect::<Vec<_>>(),
        vec![1, 2, 4],
        "x8 must be skipped when the cap is 4"
    );
    assert!(report.stop_reason.unwrap().contains("capped at 4"));
}

#[tokio::test]
async fn suite_without_eligible_tests_is_an_error() {
    let base = spawn_mock_server().await;
    let mut suite = suite_with(vec![json!({
        "name": "only-invalid",
        "payload": {},
        "expected_status": 500
    })]);
    let mut dictionary = dictionary_for(&base);

    let outcome = run_benchmark(
        &reqwest::Client::new(),
        &mut suite,
        &mut dictionary,
        BenchConfig {
            pool_size: 16,
            max_concurrency: None,
            seed: 42,
            load: None,
            escalation: true,
            max_in_flight: runner::benchmark::DEFAULT_MAX_IN_FLIGHT,
            adaptive_stop: true,
        },
    )
    .await;

    assert!(outcome.is_err());
}
