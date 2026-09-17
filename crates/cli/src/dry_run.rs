//! Static pre-flight for `--dry-run`: resolve each request's templates against
//! the environment and print what *would* be sent, without any HTTP.
//!
//! Values a real run captures from a response are not known ahead of time, so
//! each `capture` seeds a `<captured:name>` placeholder in the dictionary; that
//! way downstream requests still render in full. A `for_each` is shown as one
//! representative iteration with the item bound to `<item:as>`; its per-item
//! fields (`{{as/field}}`) are left visible but not reported as errors. Any
//! other `{{var}}` that still cannot be resolved (a typo, a missing environment
//! variable) is listed under the request.

use std::collections::HashMap;
use std::fmt::Write;

use serde_json::Value;
use vantage_core::dictionary::Dictionary;
use vantage_core::request::{ForEachSource, TestRequest};
use vantage_core::template_engine::injector::Injector;
use vantage_core::test_suite::{SuiteConfig, TestSuite};

/// Renders a static pre-flight of `suite` (no HTTP) and returns it as text.
pub fn render(suite: &mut TestSuite, dictionary: &Dictionary) -> String {
    let config: SuiteConfig = (&mut *suite).into();
    let mut vars = dictionary.variables.clone();

    let mut out = String::new();
    let file = suite.file_path.as_deref().unwrap_or("(suite)");
    let _ = writeln!(out, "Dry run: {file}");
    let _ = writeln!(out, "variables in scope: {}", sorted_keys(&vars));

    if !suite.steps.is_empty() {
        let _ = writeln!(out, "\nsteps:");
        for (i, request) in suite.steps.iter().enumerate() {
            render_request(&mut out, i + 1, request, &config, &mut vars, 1, &[]);
        }
    }
    if !suite.tests.is_empty() {
        let _ = writeln!(out, "\ntests:");
        for (i, request) in suite.tests.iter().enumerate() {
            render_request(&mut out, i + 1, request, &config, &mut vars, 1, &[]);
        }
    }

    let _ = writeln!(out, "\n(dry run - no requests sent)");
    out
}

/// `bindings` are the `for_each` item names active at this depth; placeholders
/// rooted at one of them (e.g. `{{product/sku}}`) are shown but not flagged as
/// unresolved, since their value is only known per iteration.
fn render_request(
    out: &mut String,
    index: usize,
    request: &TestRequest,
    config: &SuiteConfig,
    vars: &mut HashMap<String, Value>,
    depth: usize,
    bindings: &[String],
) {
    let mut request = request.clone();
    request.update_from(config);

    if request.for_each.is_some() {
        render_for_each(out, index, &request, config, vars, depth, bindings);
        return;
    }

    let pad = "  ".repeat(depth);
    let name = Injector::inject_str(&request.name, vars).unwrap_or_else(|| request.name.clone());
    let _ = writeln!(out, "{pad}[{index}] {name}");
    print_request_details(out, &request, vars, &pad, bindings);
    for cap_name in request.capture.keys() {
        seed_capture(cap_name, vars);
    }
}

/// Renders a `for_each` host: the loop header, one representative iteration
/// (the item bound to `<item:as>`), and the loop-level capture seeds. `host`
/// already carries the suite defaults.
fn render_for_each(
    out: &mut String,
    index: usize,
    host: &TestRequest,
    config: &SuiteConfig,
    vars: &mut HashMap<String, Value>,
    depth: usize,
    bindings: &[String],
) {
    let Some(for_each) = host.for_each.clone() else {
        return;
    };
    let pad = "  ".repeat(depth);
    let name = Injector::inject_str(&host.name, vars).unwrap_or_else(|| host.name.clone());
    let source = match &for_each.source {
        ForEachSource::Variable(v) => format!("variable `{v}`"),
        ForEachSource::Request(req) => format!("an inline request ({})", req.name),
    };
    let _ = writeln!(out, "{pad}[{index}] {name}  (for each item in {source})");

    let mut scope = iteration_scope(vars, &for_each.binding);
    let mut inner = bindings.to_vec();
    inner.push(for_each.binding.clone());

    if for_each.sequence.is_empty() {
        // The host itself is the loop body (single-call loop).
        let mut host_body = host.clone();
        host_body.for_each = None;
        print_request_details(out, &host_body, &scope, &pad, &inner);
    } else {
        for (j, body) in for_each.sequence.iter().enumerate() {
            render_request(out, j + 1, body, config, &mut scope, depth + 1, &inner);
        }
    }

    for cap_name in for_each.capture.keys() {
        seed_capture(cap_name, vars);
    }
}

/// A representative iteration scope: the item bound under `binding` as a
/// `<item:...>` placeholder and a symbolic `index`.
fn iteration_scope(vars: &HashMap<String, Value>, binding: &str) -> HashMap<String, Value> {
    let mut scope = vars.clone();
    scope.insert(
        binding.to_string(),
        Value::String(format!("<item:{binding}>")),
    );
    scope.insert("index".to_string(), Value::String("<index>".to_string()));
    scope
}

fn print_request_details(
    out: &mut String,
    request: &TestRequest,
    vars: &HashMap<String, Value>,
    pad: &str,
    bindings: &[String],
) {
    print_request_line(out, request, vars, pad);
    print_expectation(out, request, vars, pad);
    print_captures(out, request, pad);
    print_unresolved(out, request, vars, pad, bindings);
}

/// The method/URL line, then the (sorted) headers and the resolved body.
fn print_request_line(
    out: &mut String,
    request: &TestRequest,
    vars: &HashMap<String, Value>,
    pad: &str,
) {
    let method = request.method.unwrap_or_default().as_str();
    let url = request
        .url
        .as_deref()
        .map(|u| resolve_str(u, vars))
        .unwrap_or_default();
    let _ = writeln!(out, "{pad}    {method} {url}");

    let mut header_keys: Vec<&String> = request.headers.keys().collect();
    header_keys.sort();
    for key in header_keys {
        let _ = writeln!(
            out,
            "{pad}    {key}: {}",
            resolve_str(&request.headers[key], vars)
        );
    }

    if let Some(payload) = &request.payload {
        let _ = writeln!(out, "{pad}    body: {}", resolve_json(payload, vars));
    }
}

/// The expected status and, when present, the resolved expected body.
fn print_expectation(
    out: &mut String,
    request: &TestRequest,
    vars: &HashMap<String, Value>,
    pad: &str,
) {
    if let Some(expected) = &request.expected_response {
        let _ = writeln!(
            out,
            "{pad}    expect: {} {}",
            request.expected_status,
            resolve_json(expected, vars)
        );
    } else {
        let _ = writeln!(out, "{pad}    expect: {}", request.expected_status);
    }
}

/// The `capture` mappings, sorted for stable output.
fn print_captures(out: &mut String, request: &TestRequest, pad: &str) {
    if request.capture.is_empty() {
        return;
    }
    let mut caps: Vec<String> = request
        .capture
        .iter()
        .map(|(name, pointer)| format!("{name} <- {pointer:?}"))
        .collect();
    caps.sort();
    let _ = writeln!(out, "{pad}    capture: {}", caps.join(", "));
}

/// The `! unresolved:` line, when any placeholder could not be resolved.
fn print_unresolved(
    out: &mut String,
    request: &TestRequest,
    vars: &HashMap<String, Value>,
    pad: &str,
    bindings: &[String],
) {
    let unresolved = collect_unresolved(request, vars, bindings);
    if !unresolved.is_empty() {
        let _ = writeln!(out, "{pad}    ! unresolved: {}", unresolved.join(", "));
    }
}

/// Seeds `<captured:name>` into `vars` for a (possibly templated) capture name.
fn seed_capture(name: &str, vars: &mut HashMap<String, Value>) {
    let key = Injector::inject_str(name, vars).unwrap_or_else(|| name.to_string());
    let placeholder = Value::String(format!("<captured:{key}>"));
    vars.insert(key, placeholder);
}

fn resolve_str(s: &str, vars: &HashMap<String, Value>) -> String {
    Injector::inject_str(s, vars).unwrap_or_else(|| s.to_string())
}

fn resolve_json(value: &Value, vars: &HashMap<String, Value>) -> String {
    let mut resolved = value.clone();
    Injector::inject(&mut resolved, vars);
    serde_json::to_string(&resolved).unwrap_or_default()
}

fn sorted_keys(vars: &HashMap<String, Value>) -> String {
    let mut keys: Vec<&str> = vars.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys.join(", ")
}

/// Lists the `{{...}}` placeholders that remain unresolved in a request's URL,
/// headers, payload, and expected response, excluding those rooted at an active
/// `for_each` binding (their value is only known per iteration).
fn collect_unresolved(
    request: &TestRequest,
    vars: &HashMap<String, Value>,
    bindings: &[String],
) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(url) = &request.url {
        find_placeholders(&resolve_str(url, vars), &mut found);
    }
    for value in request.headers.values() {
        find_placeholders(&resolve_str(value, vars), &mut found);
    }
    if let Some(payload) = &request.payload {
        find_placeholders(&resolve_json(payload, vars), &mut found);
    }
    if let Some(expected) = &request.expected_response {
        find_placeholders(&resolve_json(expected, vars), &mut found);
    }
    found.retain(|p| !bindings.iter().any(|b| b == placeholder_root(p)));
    found.sort();
    found.dedup();
    found
}

/// The variable a placeholder body resolves against: the text before any
/// `/pointer` or `:type` suffix (e.g. `product/sku:int` -> `product`).
fn placeholder_root(inner: &str) -> &str {
    inner.split(['/', ':']).next().unwrap_or(inner).trim()
}

/// Appends the inner text of every `{{...}}` placeholder found in `s`.
fn find_placeholders(s: &str, out: &mut Vec<String>) {
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let inner = after[..end].trim();
        if !inner.is_empty() {
            out.push(inner.to_string());
        }
        rest = &after[end + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dict() -> Dictionary {
        let mut d = Dictionary::new();
        d.insert("BASE_URL".to_string(), json!("https://api.test"));
        d
    }

    #[test]
    fn captured_values_render_as_placeholders_downstream() {
        let mut suite: TestSuite = serde_json::from_value(json!({
            "url": "{{BASE_URL}}/x",
            "method": "POST",
            "steps": [
                {"name": "auth", "url": "{{BASE_URL}}/token", "payload": {},
                 "capture": {"access_token": "/access_token"}}
            ],
            "tests": [
                {"name": "call", "headers": {"Authorization": "Bearer {{access_token}}"},
                 "payload": {}, "expected_response": {}}
            ]
        }))
        .unwrap();

        let report = render(&mut suite, &dict());
        assert!(report.contains("https://api.test/token"), "{report}");
        assert!(
            report.contains("Bearer <captured:access_token>"),
            "downstream should see the captured placeholder: {report}"
        );
        assert!(report.contains("no requests sent"));
    }

    #[test]
    fn unresolved_variable_is_flagged() {
        let mut suite: TestSuite = serde_json::from_value(json!({
            "url": "{{BASE_URL}}/x",
            "tests": [{"name": "typo", "payload": {"_sku": "{{skuu}}"}, "expected_response": {}}]
        }))
        .unwrap();

        let report = render(&mut suite, &dict());
        assert!(report.contains("! unresolved: skuu"), "{report}");
    }

    #[test]
    fn for_each_sequence_renders_nested_requests_and_seeds_loop_captures() {
        let mut suite: TestSuite = serde_json::from_value(json!({
            "url": "{{BASE_URL}}/x",
            "tests": [
                {
                    "name": "hydrate",
                    "for_each": {
                        "in": "products",
                        "as": "product",
                        "sequence": [
                            {"name": "detail {{product}}", "payload": {"_sku": "{{product/sku}}"},
                             "capture": {"detail": ""}}
                        ],
                        "capture": {
                            "sku_details": {"key": "{{product/sku}}", "value": "{{detail}}"}
                        }
                    },
                    "payload": {}
                },
                {"name": "uses it", "payload": {"all": "{{sku_details}}"},
                 "expected_response": {}}
            ]
        }))
        .unwrap();

        let report = render(&mut suite, &dict());
        assert!(
            report.contains("for each item in variable `products`"),
            "{report}"
        );
        assert!(
            report.contains("detail <item:product>"),
            "the sequence body renders one representative iteration: {report}"
        );
        assert!(
            report.contains("<captured:sku_details>"),
            "downstream requests see the loop-level capture: {report}"
        );
        assert!(
            !report.contains("unresolved"),
            "nothing should be flagged: {report}"
        );
    }

    #[test]
    fn for_each_shows_item_and_does_not_flag_per_item_fields() {
        let mut suite: TestSuite = serde_json::from_value(json!({
            "url": "{{BASE_URL}}/x",
            "tests": [
                {
                    "name": "loop",
                    "for_each": {"in": "products", "as": "product"},
                    "payload": {"whole": "{{product}}", "sku": "{{product/sku}}"},
                    "expected_response": {}
                }
            ]
        }))
        .unwrap();

        let report = render(&mut suite, &dict());
        assert!(
            report.contains("for each item in variable `products`"),
            "{report}"
        );
        assert!(
            report.contains("<item:product>"),
            "whole item renders: {report}"
        );
        assert!(
            report.contains("{{product/sku}}"),
            "per-item field is shown: {report}"
        );
        assert!(
            !report.contains("unresolved"),
            "per-item field must not be flagged: {report}"
        );
    }
}
