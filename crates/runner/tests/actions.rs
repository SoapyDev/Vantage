//! Integration tests for CLI actions (before/after hooks) and the for_each
//! default iteration limit. Uses a minimal local HTTP mock server.

use runner::test_runner::TestRunner;
use runner::test_runner_default::TestRunnerDefault;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use vantage_core::dictionary::Dictionary;
use vantage_core::result::RequestType;
use vantage_core::test_suite::TestSuite;

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

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
                let body: Value =
                    serde_json::from_slice(&buf[header_end..header_end + content_length])
                        .unwrap_or(Value::Null);

                let (status, response) = route(&path, &body);
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

fn route(path: &str, body: &Value) -> (u16, Value) {
    match path {
        "/price" => {
            let sku = body["_sku"].as_str().unwrap_or("?");
            let price = match sku {
                "A" => 1.5,
                "B" => 999.0,
                "C" => 3.5,
                _ => return (500, json!({"error": "unknown sku"})),
            };
            (200, json!({"sku": sku, "unitPrice": price}))
        }
        "/many" => {
            let items: Vec<Value> = (0..150).map(|i| json!({"n": i})).collect();
            (200, json!({"items": items}))
        }
        "/ok" => (200, json!({"ok": true})),
        _ => (404, json!({"error": "not found"})),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn suite_from(value: Value) -> TestSuite {
    serde_json::from_value(value).unwrap()
}

fn base_dictionary(base_url: &str) -> Dictionary {
    let mut dictionary = Dictionary::new();
    dictionary.insert("BASE_URL".to_string(), json!(base_url));
    dictionary
}

/// Reads an action-produced file regardless of the shell that wrote it:
/// PowerShell redirection writes UTF-16LE with a BOM, sh writes UTF-8.
fn read_text(path: &str) -> String {
    let bytes = std::fs::read(path).unwrap();
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    text.replace("\r\n", "\n")
}

fn temp_file(tag: &str) -> String {
    let path = std::env::temp_dir().join(format!("vantage_actions_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().to_string()
}

async fn run_tests(
    suite: &mut TestSuite,
    dictionary: &mut Dictionary,
) -> Result<Vec<vantage_core::result::TestResult>, anyhow::Error> {
    TestRunnerDefault
        .run(&reqwest::Client::new(), suite, dictionary)
        .await
}

// ---------------------------------------------------------------------------
// Actions: before
// ---------------------------------------------------------------------------

#[tokio::test]
async fn before_action_capture_feeds_the_request() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "uses captured sku",
                "before": [
                    {"name": "pick sku", "run": "echo A", "capture": "target_sku"}
                ],
                "payload": {"_sku": "{{target_sku}}"},
                "expected_response": {"sku": "A", "unitPrice": 1.5}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await.unwrap();

    // owner line first, then its actions (Option A grouping)
    assert_eq!(results.len(), 2, "results: {results:#?}");
    let test = &results[0];
    assert_eq!(test.request_type, RequestType::Test);
    assert!(test.is_success, "error: {:?}", test.error);
    let action = &results[1];
    assert_eq!(action.request_type, RequestType::Action);
    assert!(action.is_success);
    assert!(
        action.name.starts_with("before: "),
        "before actions are labeled: {:?}",
        action.name
    );

    // capture persisted to the parent dictionary
    assert_eq!(dictionary.get("target_sku"), Some(&json!("A")));
}

#[tokio::test]
async fn capture_auto_parses_json_stdout() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/ok",
        "method": "POST",
        "tests": [
            {
                "name": "json capture",
                "before": [
                    {"name": "emit json", "run": "echo '{\"a\": 1}'", "capture": "obj"}
                ],
                "payload": {},
                "expected_response": {"ok": true}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    run_tests(&mut suite, &mut dictionary).await.unwrap();

    assert_eq!(dictionary.get("obj"), Some(&json!({"a": 1})));
}

#[tokio::test]
async fn before_fail_skips_request_and_remaining_actions() {
    let base = spawn_mock_server().await;
    let marker = temp_file("skipped");
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "never runs",
                "before": [
                    {"name": "guard", "run": "exit 1", "on_failure": "fail"},
                    {"name": "should not run", "run": format!("echo ran >> {marker}")}
                ],
                "payload": {"_sku": "A"},
                "expected_response": {"sku": "A", "unitPrice": 1.5}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await.unwrap();

    // owner line first even when a before action failed it
    let host = &results[0];
    assert_eq!(host.request_type, RequestType::Test);
    assert!(!host.is_success);
    assert!(host.status.is_none(), "request must not be executed");
    assert!(
        host.error.as_deref().unwrap_or("").contains("guard"),
        "error should mention the failing action: {:?}",
        host.error
    );

    // second action never ran
    assert!(!std::path::Path::new(&marker).exists());
}

#[tokio::test]
async fn action_abort_stops_the_suite() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "aborts",
                "before": [
                    {"name": "hard guard", "run": "exit 1", "on_failure": "abort"}
                ],
                "payload": {"_sku": "A"}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let outcome = run_tests(&mut suite, &mut dictionary).await;
    assert!(outcome.is_err(), "abort must surface as an error");
}

// ---------------------------------------------------------------------------
// Actions: after
// ---------------------------------------------------------------------------

#[tokio::test]
async fn after_action_reads_the_result_object() {
    let base = spawn_mock_server().await;
    let out = temp_file("result_obj");
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "passing test",
                "payload": {"_sku": "A"},
                "expected_response": {"sku": "A", "unitPrice": 1.5},
                "after": [
                    {
                        "name": "log result",
                        "run": format!("echo '{{{{result/body/unitPrice}}}},{{{{result/status}}}},{{{{result/is_success}}}}' >> {out}")
                    }
                ]
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await.unwrap();
    assert!(results.iter().all(|r| r.is_success), "{results:#?}");

    // owner first, labeled after action second
    assert_eq!(results[0].request_type, RequestType::Test);
    assert!(
        results[1].name.starts_with("after: "),
        "{:?}",
        results[1].name
    );

    let content = read_text(&out);
    assert_eq!(content.trim(), "1.5,200,true");
    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn after_actions_run_even_when_the_test_fails() {
    let base = spawn_mock_server().await;
    let out = temp_file("on_fail");
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "failing test",
                "payload": {"_sku": "B"},
                "expected_response": {"sku": "B", "unitPrice": 2.5},
                "after": [
                    {
                        "name": "log failure",
                        "run": format!("echo '{{{{result/is_success}}}}' >> {out}")
                    }
                ]
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await.unwrap();

    let host = results
        .iter()
        .find(|r| r.request_type == RequestType::Test)
        .unwrap();
    assert!(!host.is_success, "mock returns a wrong price for B");

    let content = read_text(&out);
    assert_eq!(content.trim(), "false");
    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn failing_continue_action_keeps_the_test_verdict() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "passing test",
                "payload": {"_sku": "A"},
                "expected_response": {"sku": "A", "unitPrice": 1.5},
                "after": [
                    {"name": "broken cleanup", "run": "exit 7"}
                ]
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await.unwrap();

    let host = results
        .iter()
        .find(|r| r.request_type == RequestType::Test)
        .unwrap();
    assert!(
        host.is_success,
        "continue failure must not flip the verdict"
    );

    let action = results
        .iter()
        .find(|r| r.request_type == RequestType::Action)
        .unwrap();
    assert!(!action.is_success);
}

#[tokio::test]
async fn after_fail_action_flips_the_test_verdict() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "passing test",
                "payload": {"_sku": "A"},
                "expected_response": {"sku": "A", "unitPrice": 1.5},
                "after": [
                    {"name": "post assertion", "run": "exit 1", "on_failure": "fail"}
                ]
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await.unwrap();

    let host = results
        .iter()
        .find(|r| r.request_type == RequestType::Test)
        .unwrap();
    assert!(!host.is_success, "after fail action must flip the verdict");
}

// ---------------------------------------------------------------------------
// Actions inside for_each
// ---------------------------------------------------------------------------

#[tokio::test]
async fn actions_see_the_iteration_scope() {
    let base = spawn_mock_server().await;
    let out = temp_file("loop_scope");
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "tests": [
            {
                "name": "loop - {{sku}}",
                "for_each": {
                    "in": "products",
                    "as": "sku"
                },
                "payload": {"_sku": "{{sku}}"},
                "after": [
                    {
                        "name": "log {{sku}}",
                        "run": format!("echo '{{{{sku}}}},{{{{result/status}}}}' >> {out}")
                    }
                ]
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);
    dictionary.insert("products".to_string(), json!(["A", "C"]));

    run_tests(&mut suite, &mut dictionary).await.unwrap();

    let content = read_text(&out);
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines, vec!["A,200", "C,200"]);
    let _ = std::fs::remove_file(&out);
}

// ---------------------------------------------------------------------------
// for_each default limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn for_each_defaults_to_100_iterations() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/ok",
        "method": "POST",
        "tests": [
            {
                "name": "n {{item/n}}",
                "for_each": {
                    "in": {
                        "name": "list",
                        "url": "{{BASE_URL}}/many",
                        "method": "POST",
                        "payload": {}
                    },
                    "items_path": "/items"
                },
                "payload": {},
                "expected_response": {"ok": true}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await.unwrap();

    let test_count = results
        .iter()
        .filter(|r| r.request_type == RequestType::Test)
        .count();
    assert_eq!(test_count, 100, "default limit must cap at 100 iterations");
}
