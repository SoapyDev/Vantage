//! Integration tests for compare mode: every request is executed against two
//! environments (lhs = `variables`, rhs = `compare_variables`) and the two
//! responses are compared to each other.

use runner::step_runner::StepRunner;
use runner::step_runner_compare::StepRunnerCompare;
use runner::test_runner::TestRunner;
use runner::test_runner_compare::TestRunnerCompare;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use vantage_core::dictionary::Dictionary;
use vantage_core::result::RequestType;
use vantage_core::test_suite::TestSuite;

// ---------------------------------------------------------------------------
// Mock server (parameterized per side)
// ---------------------------------------------------------------------------

async fn spawn_mock_server(token: &'static str, price: f64, price_status: u16) -> String {
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

                let body: Value =
                    serde_json::from_slice(&buf[header_end..header_end + content_length])
                        .unwrap_or(Value::Null);

                let (status, response) = match path.as_str() {
                    "/token" => (200, json!({"access_token": token})),
                    "/price" => (price_status, json!({"unitPrice": price})),
                    // Echoes the received payload back, so tests can observe
                    // what each side actually sent.
                    "/echo" => (200, body),
                    _ => (404, json!({"error": "not found"})),
                };

                let response_bytes = serde_json::to_vec(&response).unwrap();
                let head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_bytes.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&response_bytes).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    format!("http://{addr}")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn suite_from(value: Value) -> TestSuite {
    serde_json::from_value(value).unwrap()
}

fn compare_dictionary(lhs_base: &str, rhs_base: &str) -> Dictionary {
    let mut dictionary = Dictionary::new();
    dictionary
        .variables
        .insert("BASE_URL".to_string(), json!(lhs_base));
    dictionary
        .compare_variables
        .insert("BASE_URL".to_string(), json!(rhs_base));
    dictionary
}

fn price_test_suite() -> Value {
    json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {"name": "price matches across environments", "payload": {}}
        ]
    })
}

// ---------------------------------------------------------------------------
// TestRunnerCompare
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compare_passes_when_both_sides_match() {
    let lhs = spawn_mock_server("tok-lhs", 1.5, 200).await;
    let rhs = spawn_mock_server("tok-rhs", 1.5, 200).await;
    let mut suite = suite_from(price_test_suite());
    let mut dictionary = compare_dictionary(&lhs, &rhs);

    let results = TestRunnerCompare
        .run(&reqwest::Client::new(), &mut suite, &mut dictionary)
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert!(results[0].is_success, "error: {:?}", results[0].error);
}

#[tokio::test]
async fn compare_fails_when_bodies_differ() {
    let lhs = spawn_mock_server("tok-lhs", 1.5, 200).await;
    let rhs = spawn_mock_server("tok-rhs", 9.9, 200).await;
    let mut suite = suite_from(price_test_suite());
    let mut dictionary = compare_dictionary(&lhs, &rhs);

    let results = TestRunnerCompare
        .run(&reqwest::Client::new(), &mut suite, &mut dictionary)
        .await
        .unwrap();

    assert!(!results[0].is_success, "differing bodies must fail");
}

#[tokio::test]
async fn compare_fails_when_statuses_differ() {
    let lhs = spawn_mock_server("tok-lhs", 1.5, 200).await;
    let rhs = spawn_mock_server("tok-rhs", 1.5, 500).await;
    let mut suite = suite_from(price_test_suite());
    let mut dictionary = compare_dictionary(&lhs, &rhs);

    let results = TestRunnerCompare
        .run(&reqwest::Client::new(), &mut suite, &mut dictionary)
        .await
        .unwrap();

    assert!(!results[0].is_success, "differing statuses must fail");
    assert!(
        results[0].error.as_deref().unwrap_or("").contains("Status"),
        "error: {:?}",
        results[0].error
    );
}

#[tokio::test]
async fn compare_resolves_payload_templates_per_side() {
    let lhs = spawn_mock_server("tok-lhs", 1.5, 200).await;
    let rhs = spawn_mock_server("tok-rhs", 1.5, 200).await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/echo",
        "method": "POST",
        "tests": [
            {"name": "echo {{ENV_NAME}}", "payload": {"env": "{{ENV_NAME}}"}}
        ]
    }));
    let mut dictionary = compare_dictionary(&lhs, &rhs);
    dictionary
        .variables
        .insert("ENV_NAME".to_string(), json!("primary"));
    dictionary
        .compare_variables
        .insert("ENV_NAME".to_string(), json!("compare"));

    let results = TestRunnerCompare
        .run(&reqwest::Client::new(), &mut suite, &mut dictionary)
        .await
        .unwrap();

    assert_eq!(
        results[0].body,
        Some(json!({"env": "primary"})),
        "lhs payload must resolve with the primary variables"
    );
    assert_eq!(
        results[0].expected_body,
        Some(json!({"env": "compare"})),
        "rhs payload must resolve with the compare variables, not the primary ones"
    );
    assert_eq!(
        results[0].name, "echo primary",
        "the result name must resolve against the primary dictionary"
    );
}

#[tokio::test]
async fn compare_mode_rejects_for_each() {
    let lhs = spawn_mock_server("tok-lhs", 1.5, 200).await;
    let rhs = spawn_mock_server("tok-rhs", 1.5, 200).await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "looped",
                "for_each": {"in": "items"},
                "payload": {}
            }
        ]
    }));
    let mut dictionary = compare_dictionary(&lhs, &rhs);

    let outcome = TestRunnerCompare
        .run(&reqwest::Client::new(), &mut suite, &mut dictionary)
        .await;

    assert!(
        outcome.is_err(),
        "for_each must be rejected in compare mode"
    );
}

// ---------------------------------------------------------------------------
// StepRunnerCompare
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_compare_extracts_into_both_dictionaries() {
    let lhs = spawn_mock_server("tok-lhs", 1.5, 200).await;
    let rhs = spawn_mock_server("tok-rhs", 1.5, 200).await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "steps": [
            {
                "name": "auth",
                "url": "{{BASE_URL}}/token",
                "method": "POST",
                "payload": {},
                "capture": {"access_token": "/access_token"}
            }
        ],
        "tests": []
    }));
    let mut dictionary = compare_dictionary(&lhs, &rhs);

    let results = StepRunnerCompare
        .run(&reqwest::Client::new(), &mut suite, &mut dictionary)
        .await
        .unwrap();

    assert!(results[0].is_success, "error: {:?}", results[0].error);
    // each side keeps its own token
    assert_eq!(
        dictionary.variables.get("access_token"),
        Some(&json!("tok-lhs"))
    );
    assert_eq!(
        dictionary.compare_variables.get("access_token"),
        Some(&json!("tok-rhs"))
    );
}

// ---------------------------------------------------------------------------
// Hooks in compare mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compare_hooks_capture_into_both_dictionaries() {
    let lhs = spawn_mock_server("tok-lhs", 1.5, 200).await;
    let rhs = spawn_mock_server("tok-rhs", 1.5, 200).await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "with hooks",
                "before": [
                    {"name": "mark", "run": "echo marked", "capture": "marker"}
                ],
                "payload": {}
            }
        ]
    }));
    let mut dictionary = compare_dictionary(&lhs, &rhs);

    let results = TestRunnerCompare
        .run(&reqwest::Client::new(), &mut suite, &mut dictionary)
        .await
        .unwrap();

    // owner line + one action result per side
    let action_count = results
        .iter()
        .filter(|r| r.request_type == RequestType::Action)
        .count();
    assert_eq!(action_count, 2, "before hooks run once per side");

    assert_eq!(dictionary.variables.get("marker"), Some(&json!("marked")));
    assert_eq!(
        dictionary.compare_variables.get("marker"),
        Some(&json!("marked"))
    );
}
