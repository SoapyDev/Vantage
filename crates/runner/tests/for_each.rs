//! Integration tests for the for_each looping strategy.
//! Uses a minimal local HTTP mock server (tokio only, no extra deps).

use runner::step_runner::StepRunner;
use runner::step_runner_default::StepRunnerDefault;
use runner::test_runner::TestRunner;
use runner::test_runner_default::TestRunnerDefault;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use vantage_core::dictionary::Dictionary;
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
                // Read until end of headers
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

fn product(sku: &str, price: f64) -> Value {
    json!({"sku": sku, "price": price, "currency": "CAD"})
}

fn route(path: &str, body: &Value) -> (u16, Value) {
    match path {
        "/token" => (200, json!({"access_token": "tok-123"})),
        "/products" => (
            200,
            json!({"products": [product("A", 1.5), product("B", 2.5), product("C", 3.5)]}),
        ),
        "/empty" => (200, json!({"products": []})),
        "/notarray" => (200, json!({"products": {"oops": true}})),
        "/detail" => {
            let sku = body["_sku"].as_str().unwrap_or("?");
            let price = match sku {
                "A" => 1.5,
                "B" => 2.5,
                "C" => 3.5,
                _ => return (500, json!({"error": "unknown sku"})),
            };
            (200, product(sku, price))
        }
        "/price" => {
            let sku = body["_sku"].as_str().unwrap_or("?");
            // "B" intentionally returns a wrong price to produce a FAIL
            let price = match sku {
                "A" => 1.5,
                "B" => 999.0,
                "C" => 3.5,
                _ => return (500, json!({"error": "unknown sku"})),
            };
            (200, json!({"sku": sku, "unitPrice": price}))
        }
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

async fn run_steps(
    suite: &mut TestSuite,
    dictionary: &mut Dictionary,
) -> Vec<vantage_core::result::TestResult> {
    StepRunnerDefault
        .run(&reqwest::Client::new(), suite, dictionary)
        .await
        .unwrap()
}

async fn run_tests(
    suite: &mut TestSuite,
    dictionary: &mut Dictionary,
) -> Vec<vantage_core::result::TestResult> {
    TestRunnerDefault
        .run(&reqwest::Client::new(), suite, dictionary)
        .await
        .unwrap()
}

fn hydrate_suite(items_url: &str) -> Value {
    json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "steps": [
            {
                "name": "List products",
                "url": format!("{{{{BASE_URL}}}}{items_url}"),
                "method": "POST",
                "payload": {},
                "capture": {"products": "/products"}
            },
            {
                "name": "Hydrate product details",
                "for_each": {
                    "in": "products",
                    "as": "product",
                    "sequence": [
                        {
                            "name": "Get detail - {{product/sku}}",
                            "url": "{{BASE_URL}}/detail",
                            "method": "POST",
                            "payload": {"_sku": "{{product/sku}}"},
                            "capture": {"detail": ""}
                        }
                    ],
                    "capture": {
                        "sku_details": {"key": "{{product/sku}}", "value": "{{detail}}"}
                    }
                }
            }
        ],
        "tests": []
    })
}

// ---------------------------------------------------------------------------
// Step loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_for_each_runs_body_per_item_and_captures() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(hydrate_suite("/products"));
    let mut dictionary = base_dictionary(&base);

    let results = run_steps(&mut suite, &mut dictionary).await;

    // 1 list step + 3 loop iterations
    assert_eq!(results.len(), 4, "results: {results:#?}");
    assert!(
        results.iter().all(|r| r.is_success),
        "results: {results:#?}"
    );

    // Iteration names are injected
    assert_eq!(results[1].name, "Get detail - A");
    assert_eq!(results[3].name, "Get detail - C");

    // capture built the map in the parent dictionary
    let details = dictionary.get("sku_details").unwrap();
    assert_eq!(details["A"]["price"], json!(1.5));
    assert_eq!(details["B"]["price"], json!(2.5));
    assert_eq!(details["C"]["currency"], json!("CAD"));

    // iteration-scoped variables must NOT leak into the parent dictionary
    assert!(dictionary.get("product").is_none());
    assert!(dictionary.get("detail").is_none());
}

#[tokio::test]
async fn step_for_each_with_empty_list_yields_no_results() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(hydrate_suite("/empty"));
    let mut dictionary = base_dictionary(&base);

    let results = run_steps(&mut suite, &mut dictionary).await;

    assert_eq!(
        results.len(),
        1,
        "only the list step should produce a result"
    );
    assert!(results[0].is_success);
}

#[tokio::test]
async fn step_for_each_on_non_array_yields_single_failure() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(hydrate_suite("/notarray"));
    let mut dictionary = base_dictionary(&base);

    let results = run_steps(&mut suite, &mut dictionary).await;

    assert_eq!(results.len(), 2);
    assert!(!results[1].is_success);
    assert!(results[1].error.as_deref().unwrap_or("").contains("array"));
}

#[tokio::test]
async fn step_for_each_respects_limit() {
    let base = spawn_mock_server().await;
    let mut value = hydrate_suite("/products");
    value["steps"][1]["for_each"]["limit"] = json!(2);
    let mut suite = suite_from(value);
    let mut dictionary = base_dictionary(&base);

    let results = run_steps(&mut suite, &mut dictionary).await;

    assert_eq!(results.len(), 3, "1 list step + 2 limited iterations");
    let details = dictionary.get("sku_details").unwrap();
    assert!(details.get("A").is_some());
    assert!(details.get("B").is_some());
    assert!(details.get("C").is_none());
}

#[tokio::test]
async fn step_for_each_with_request_source() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "steps": [
            {
                "name": "Hydrate product details",
                "for_each": {
                    "in": {
                        "name": "List products",
                        "url": "{{BASE_URL}}/products",
                        "method": "POST",
                        "payload": {}
                    },
                    "items_path": "/products",
                    "as": "product",
                    "sequence": [
                        {
                            "name": "Get detail - {{product/sku}}",
                            "url": "{{BASE_URL}}/detail",
                            "method": "POST",
                            "payload": {"_sku": "{{product/sku}}"},
                            "capture": {"detail": ""}
                        }
                    ],
                    "capture": {
                        "sku_details": {"key": "{{product/sku}}", "value": "{{detail}}"}
                    }
                }
            }
        ],
        "tests": []
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_steps(&mut suite, &mut dictionary).await;

    // 1 source request + 3 iterations
    assert_eq!(results.len(), 4, "results: {results:#?}");
    let details = dictionary.get("sku_details").unwrap();
    assert_eq!(details["B"]["price"], json!(2.5));
}

// ---------------------------------------------------------------------------
// Test loop
// ---------------------------------------------------------------------------

fn looped_tests_suite() -> Value {
    json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "steps": [],
        "tests": [
            {
                "name": "Generic - known product returns a price",
                "payload": {"_sku": "A"},
                "expected_response": {"sku": "A", "unitPrice": 1.5}
            },
            {
                "name": "Price matches detail - {{sku}}",
                "for_each": {
                    "in": "products",
                    "as": "sku"
                },
                "payload": {"_sku": "{{sku}}"},
                "expected_response": {
                    "sku": "{{sku}}",
                    "unitPrice": "{{sku_details/${sku}/price:float}}"
                }
            }
        ]
    })
}

fn dictionary_with_products(base: &str) -> Dictionary {
    let mut dictionary = base_dictionary(base);
    dictionary.insert("products".to_string(), json!(["A", "B", "C"]));
    dictionary.insert(
        "sku_details".to_string(),
        json!({
            "A": {"price": 1.5, "currency": "CAD"},
            "B": {"price": 2.5, "currency": "CAD"},
            "C": {"price": 3.5, "currency": "CAD"}
        }),
    );
    dictionary
}

#[tokio::test]
async fn test_for_each_expands_and_compares_each_item() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(looped_tests_suite());
    let mut dictionary = dictionary_with_products(&base);

    let results = run_tests(&mut suite, &mut dictionary).await;

    // 1 generic + 3 expanded
    assert_eq!(results.len(), 4, "results: {results:#?}");

    assert!(
        results[0].is_success,
        "generic test should pass: {:?}",
        results[0].error
    );
    let by_name: HashMap<&str, bool> = results
        .iter()
        .map(|r| (r.name.as_str(), r.is_success))
        .collect();
    assert_eq!(by_name.get("Price matches detail - A"), Some(&true));
    // mock server intentionally returns a wrong price for B
    assert_eq!(by_name.get("Price matches detail - B"), Some(&false));
    assert_eq!(by_name.get("Price matches detail - C"), Some(&true));
}

#[tokio::test]
async fn plain_test_capture_persists_into_dictionary() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/token",
        "method": "POST",
        "steps": [],
        "tests": [
            {
                "name": "auth as test",
                "payload": {},
                "capture": {"bearer_token": "/access_token"},
                "expected_response": {"access_token": "tok-123"}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await;

    assert!(results[0].is_success, "error: {:?}", results[0].error);
    assert_eq!(
        dictionary.variables.get("bearer_token"),
        Some(&json!("tok-123")),
        "capture on a plain test must persist into the dictionary, like steps do"
    );
}

#[tokio::test]
async fn expected_response_is_injected_for_plain_tests() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "steps": [],
        "tests": [
            {
                "name": "Injected expected",
                "payload": {"_sku": "A"},
                "expected_response": {"sku": "{{known_sku}}", "unitPrice": "{{known_price:float}}"}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);
    dictionary.insert("known_sku".to_string(), json!("A"));
    dictionary.insert("known_price".to_string(), json!("1.5"));

    let results = run_tests(&mut suite, &mut dictionary).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].is_success, "error: {:?}", results[0].error);
}

// ---------------------------------------------------------------------------
// Template resolution errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unresolved_variable_fails_the_request_with_available_hint() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "steps": [],
        "tests": [
            {
                "name": "typo",
                "payload": {"_sku": "{{skuu}}"},
                "expected_response": {"sku": "A", "unitPrice": 1.5}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await;

    assert_eq!(results.len(), 1);
    assert!(
        !results[0].is_success,
        "an unresolved variable must fail the request"
    );
    let error = results[0].error.as_deref().unwrap_or("");
    assert!(
        error.contains("skuu"),
        "error should name the missing variable: {error}"
    );
    assert!(
        error.contains("BASE_URL"),
        "error should list the available variables: {error}"
    );
}

#[tokio::test]
async fn missing_capture_pointer_warns_but_does_not_fail() {
    let base = spawn_mock_server().await;
    let mut suite = suite_from(json!({
        "url": "{{BASE_URL}}/token",
        "method": "POST",
        "steps": [],
        "tests": [
            {
                "name": "auth",
                "payload": {},
                "capture": {"tok": "/nope"},
                "expected_response": {"access_token": "tok-123"}
            }
        ]
    }));
    let mut dictionary = base_dictionary(&base);

    let results = run_tests(&mut suite, &mut dictionary).await;

    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_success,
        "a capture miss must not fail a request whose assertion held: {:?}",
        results[0].error
    );
    assert!(
        dictionary.get("tok").is_none(),
        "an unresolved capture pointer must not insert a value"
    );
}
