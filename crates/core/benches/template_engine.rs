//! Micro-benches for the template engine: the hottest CPU path of every
//! run (each request injects its URL, payload and headers, then extracts
//! values from the response).
//!
//! Compare before/after a change with:
//! `cargo bench -p vantage-core -- --save-baseline before` then
//! `cargo bench -p vantage-core -- --baseline before`.

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};
use vantage_core::request::TestRequest;
use vantage_core::template_engine::extractor::Extractor;
use vantage_core::template_engine::injector::Injector;

/// A dictionary shaped like a real run: environment variables plus values
/// extracted by previous steps.
fn dictionary() -> HashMap<String, Value> {
    let mut dict = HashMap::new();
    dict.insert("BASE_URL".to_string(), json!("https://api.example.com"));
    dict.insert("bearer_token".to_string(), json!("tok-123456789"));
    dict.insert("sku".to_string(), json!("100393-501"));
    dict.insert("qty".to_string(), json!("3"));
    dict.insert(
        "sku_details".to_string(),
        json!({
            "100393-501": {"price": 19.07, "currency": "CAD"},
            "100393-502": {"price": 21.50, "currency": "CAD"}
        }),
    );
    dict
}

/// A payload exercising every injector feature: plain placeholders, casts,
/// pointer reads, dynamic segments, nesting and arrays.
fn payload_template() -> Value {
    json!({
        "_sku": "{{sku}}",
        "quantity": "{{qty:int}}",
        "expected": "{{sku_details/${sku}/price:float}}",
        "nested": {
            "url": "{{BASE_URL}}/detail",
            "items": ["{{sku}}", "{{qty}}", "static"]
        },
        "static_part": {"a": 1, "b": [true, null, 2.5]}
    })
}

fn bench_inject_payload(c: &mut Criterion) {
    let dict = dictionary();
    let template = payload_template();

    c.bench_function("inject_nested_payload", |b| {
        b.iter(|| {
            let mut value = template.clone();
            Injector::inject(&mut value, black_box(&dict));
            value
        });
    });
}

fn bench_inject_str_url(c: &mut Criterion) {
    let dict = dictionary();

    c.bench_function("inject_str_url", |b| {
        b.iter(|| {
            Injector::inject_str(
                black_box("{{BASE_URL}}/api/services/MyService/getPrice"),
                &dict,
            )
        });
    });
}

fn bench_extract_from_body(c: &mut Criterion) {
    let request: TestRequest = serde_json::from_value(json!({
        "name": "bench",
        "capture": {
            "access_token": "/access_token",
            "{{sku}}-detail": "/detail",
            "whole_body": ""
        }
    }))
    .expect("valid bench request");
    let body = json!({
        "access_token": "tok-abcdef",
        "detail": {"price": 19.07, "currency": "CAD"},
        "noise": [1, 2, 3, 4, 5]
    });

    c.bench_function("extract_three_pointers", |b| {
        b.iter(|| {
            let mut dict = dictionary();
            Extractor::extract(black_box(&body), &request, &mut dict);
            dict
        });
    });
}

criterion_group!(
    benches,
    bench_inject_payload,
    bench_inject_str_url,
    bench_extract_from_body
);
criterion_main!(benches);
