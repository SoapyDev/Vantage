//! Macro-benches for the request pipeline: suite parsing, benchmark pool
//! building, and a full run (inject -> HTTP -> grade -> extract) against a
//! local [`mock_server`] so the network can never skew the numbers.
//!
//! Compare before/after a change with:
//! `cargo bench -p runner -- --save-baseline before` then
//! `cargo bench -p runner -- --baseline before`.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use mock_server::{MockResponse, MockServer};
use runner::benchmark::select_pool;
use runner::test_runner::TestRunner;
use runner::test_runner_default::TestRunnerDefault;
use serde_json::{Value, json};
use vantage_core::dictionary::Dictionary;
use vantage_core::test_suite::TestSuite;

/// A suite of `tests` identical priced requests, templated like real ones.
fn suite_value(tests: usize) -> Value {
    let tests: Vec<Value> = (0..tests)
        .map(|i| {
            json!({
                "name": format!("priced {i}"),
                "payload": {"_sku": "{{sku}}", "qty": "{{qty:int}}"},
                "expected_response": {"unitPrice": 1.5}
            })
        })
        .collect();
    json!({
        "url": "{{BASE_URL}}/price",
        "method": "POST",
        "steps": [],
        "tests": tests
    })
}

fn parse_suite(tests: usize) -> TestSuite {
    serde_json::from_value(suite_value(tests)).expect("valid bench suite")
}

fn bench_parse_suite(c: &mut Criterion) {
    let raw = serde_json::to_string(&suite_value(50)).expect("serializable suite");

    c.bench_function("parse_suite_50_tests", |b| {
        b.iter(|| serde_json::from_str::<TestSuite>(black_box(&raw)).expect("parse"));
    });
}

fn bench_select_pool(c: &mut Criterion) {
    let suite = parse_suite(100);

    c.bench_function("select_pool_100_tests_into_128", |b| {
        b.iter(|| select_pool(black_box(&suite.tests), 128, 42));
    });
}

fn bench_full_run(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("bench runtime");
    let server = runtime.block_on(
        MockServer::builder()
            .route("/price", MockResponse::json(200, json!({"unitPrice": 1.5})))
            .spawn(),
    );
    let base_url = server.base_url().to_string();
    let client = reqwest::Client::new();

    c.bench_function("pipeline_run_10_tests", |b| {
        b.to_async(&runtime).iter_batched(
            || {
                let mut dictionary = Dictionary::new();
                dictionary.insert("BASE_URL".to_string(), json!(base_url.clone()));
                dictionary.insert("sku".to_string(), json!("100393-501"));
                dictionary.insert("qty".to_string(), json!("3"));
                (parse_suite(10), dictionary)
            },
            |(mut suite, mut dictionary)| {
                let client = client.clone();
                async move {
                    TestRunnerDefault
                        .run(&client, &mut suite, &mut dictionary)
                        .await
                        .expect("bench run")
                }
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    bench_parse_suite,
    bench_select_pool,
    bench_full_run
);
criterion_main!(benches);
