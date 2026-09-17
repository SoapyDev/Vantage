//! Test execution engine: turns a [`TestSuite`](vantage_core::test_suite::TestSuite)
//! into HTTP requests, runs them (sequentially, looped, or in compare mode),
//! and produces [`TestResult`]s.
//!
//! This crate owns the binding to [`reqwest`]: [`vantage_core`] stays free of any
//! HTTP dependency, so the mapping from the suite-level types onto reqwest
//! lives here (see the private `to_reqwest_method` and `make_request`).

use anyhow::anyhow;
use reqwest::RequestBuilder;
use serde_json::Value;
use std::{collections::HashMap, error::Error};
use vantage_core::{
    HttpContentType, HttpMethod,
    request::TestRequest,
    result::{RequestType, TestResult},
};

use vantage_core::template_engine::injector::{Injector, TemplateError};

pub mod action_hooks;
pub mod benchmark;
pub(crate) mod comparator;
pub(crate) mod for_each;
pub(crate) mod sequence;
pub mod step_runner;
pub mod step_runner_compare;
pub mod step_runner_default;
pub mod test_runner;
pub mod test_runner_compare;
pub mod test_runner_default;

/// Maps the suite-level HTTP method onto reqwest's. Lives here so that
/// vantage-core stays free of HTTP dependencies.
fn to_reqwest_method(method: HttpMethod) -> reqwest::Method {
    match method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Head => reqwest::Method::HEAD,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
        HttpMethod::Patch => reqwest::Method::PATCH,
        HttpMethod::Delete => reqwest::Method::DELETE,
        HttpMethod::Options => reqwest::Method::OPTIONS,
        HttpMethod::Trace => reqwest::Method::TRACE,
    }
}

/// Builds a clear error for an unresolved template placeholder in a request.
/// For an unknown variable it also lists the variables that *are* in scope,
/// which is the most common source of the mistake (a typo).
pub(crate) fn unresolved_template(
    error: TemplateError,
    request_name: &str,
    dictionary: &HashMap<String, Value>,
) -> anyhow::Error {
    match &error {
        TemplateError::UnknownVariable { .. } => {
            let mut available: Vec<&str> = dictionary.keys().map(String::as_str).collect();
            available.sort_unstable();
            anyhow!(
                "{error} in request '{request_name}' (available: {})",
                available.join(", ")
            )
        }
        TemplateError::UnknownPointer { .. } => anyhow!("{error} in request '{request_name}'"),
    }
}

/// Builds the reqwest request for a single test/step, resolving templates in
/// the URL, payload, and headers against `dictionary`.
///
/// # Errors
///
/// Returns an error when the request carries a payload with an unsupported
/// content type (currently `multipart/form-data`), so the caller can surface a
/// clear failure instead of the engine panicking mid-run.
pub(crate) fn make_request(
    client: &reqwest::Client,
    request_content: &mut TestRequest,
    dictionary: &HashMap<String, Value>,
) -> Result<RequestBuilder, anyhow::Error> {
    let raw_url = request_content.url.as_deref().unwrap_or_default();
    let url = Injector::inject_str_checked(raw_url, dictionary)
        .map_err(|e| unresolved_template(e, &request_content.name, dictionary))?;

    let method = to_reqwest_method(request_content.method.unwrap_or_default());
    let content_type = request_content.content_type.to_string();

    let mut request = client.request(method, url);
    request = request.header("Content-Type", content_type);

    if let Some(payload) = request_content.payload.as_mut() {
        Injector::inject_checked(payload, dictionary)
            .map_err(|e| unresolved_template(e, &request_content.name, dictionary))?;

        request = match request_content.content_type {
            HttpContentType::Json => request.json(&payload),
            HttpContentType::FormURLEncoded => request.form(&payload),
            HttpContentType::MultipartFormData => {
                return Err(anyhow!(
                    "multipart/form-data payloads are not supported yet (request '{}')",
                    request_content.name
                ));
            }
        };
    }

    for (key, value) in &request_content.headers {
        let mut val = Value::String(value.to_owned());
        Injector::inject_checked(&mut val, dictionary)
            .map_err(|e| unresolved_template(e, &request_content.name, dictionary))?;
        let value = val.as_str().map(ToOwned::to_owned).unwrap_or_default();
        request = request.header(key, value);
    }

    Ok(request)
}

/// Sends the built request and turns the outcome (response or transport
/// error) into a timed [`TestResult`].
async fn handle_request(
    request: &TestRequest,
    builder: RequestBuilder,
    request_type: RequestType,
) -> TestResult {
    let mut result: TestResult = request.into();
    result.request_type = request_type;

    let start = std::time::Instant::now();
    let response = builder.send().await;
    result.duration = start.elapsed().as_millis();

    match response {
        Ok(response) => record_response(&mut result, response, request).await,
        Err(e) => {
            result.error = e.source().map(ToString::to_string);
            result.status = e.status().map(|s| s.as_u16());
        }
    }

    result
}

/// Records a received response into `result`: status, headers, and the JSON
/// body after the request's `ignored_fields` and `sorts` normalizations.
async fn record_response(
    result: &mut TestResult,
    response: reqwest::Response,
    request: &TestRequest,
) {
    result.status = Some(response.status().as_u16());
    result.headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect::<HashMap<_, _>>();

    if let Ok(mut body) = response.json().await {
        remove_header_field(&mut body);
        remove_json_fields(&mut body, &request.ignored_fields);
        apply_json_sorts(&mut body, &request.sorts);
        result.body = Some(body);
    }
}

fn remove_header_field(value: &mut Value) {
    if let Some(obj) = value.as_object_mut() {
        obj.remove("headers");
    }
}

fn remove_json_fields(value: &mut Value, paths: &[String]) {
    for path in paths {
        remove_json_field(value, path);
    }
}

fn remove_json_field(value: &mut Value, path: &str) {
    fn remove_field_recursive(value: &mut Value, parts: &[&str]) {
        if parts.is_empty() {
            return;
        }

        match value {
            Value::Object(obj) => {
                if parts.len() == 1 {
                    obj.remove(parts[0]);
                } else if let Some(child) = obj.get_mut(parts[0]) {
                    remove_field_recursive(child, &parts[1..]);
                }

                for child in obj.values_mut() {
                    remove_field_recursive(child, parts);
                }
            }
            Value::Array(items) => {
                for item in items {
                    remove_field_recursive(item, parts);
                }
            }
            _ => {}
        }
    }

    let parts: Vec<&str> = path.split('.').collect();
    remove_field_recursive(value, &parts);
}

fn apply_json_sorts(value: &mut Value, sorts: &[String]) {
    for sort_path in sorts {
        apply_json_sort(value, sort_path);
    }
}

fn apply_json_sort(value: &mut Value, sort_path: &str) {
    let path = sort_path.strip_prefix("$.").unwrap_or(sort_path);
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return;
    }

    sort_first_matching_array_field(value, &parts);
}

fn sort_first_matching_array_field(value: &mut Value, parts: &[&str]) -> bool {
    match value {
        Value::Object(obj) => {
            if parts.len() == 1
                && let Some(Value::Array(items)) = obj.get_mut(parts[0])
            {
                items.sort_by(|a, b| compare_json_values(a.get(parts[0]), b.get(parts[0])));
                return true;
            }

            if parts.len() > 1
                && let Some(child) = obj.get_mut(parts[0])
                && sort_first_matching_array_field(child, &parts[1..])
            {
                return true;
            }

            for child in obj.values_mut() {
                if sort_first_matching_array_field(child, parts) {
                    return true;
                }
            }

            false
        }
        Value::Array(items) => {
            if let Some(first_part) = parts.first() {
                for item in items.iter_mut() {
                    if let Value::Object(obj) = item
                        && parts.len() == 1
                        && obj.contains_key(*first_part)
                    {
                        // We reached the field; sort THIS array by that
                        // field. (Re-borrowing `items` during iteration is
                        // accepted because this path returns immediately: the
                        // iterator is dead when `sort_by` borrows.)
                        items.sort_by(|a, b| {
                            compare_json_values(a.get(first_part), b.get(first_part))
                        });
                        return true;
                    }
                    if sort_first_matching_array_field(item, parts) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

fn compare_json_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (a, b) {
        (Some(Value::String(a)), Some(Value::String(b))) => a.cmp(b),
        (Some(Value::Number(a)), Some(Value::Number(b))) => {
            let a = a.as_f64().unwrap_or(f64::NAN);
            let b = b.as_f64().unwrap_or(f64::NAN);
            a.partial_cmp(&b).unwrap_or(Ordering::Equal)
        }
        (Some(Value::Bool(a)), Some(Value::Bool(b))) => a.cmp(b),
        (Some(a), Some(b)) => a.to_string().cmp(&b.to_string()),
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- make_request ----------

    fn request_with(content_type: &str, payload: Option<Value>) -> TestRequest {
        let mut value = json!({
            "name": "multipart upload",
            "url": "http://example.test/upload",
            "content_type": content_type,
        });
        if let Some(payload) = payload {
            value["payload"] = payload;
        }
        serde_json::from_value(value).expect("valid TestRequest fixture")
    }

    #[test]
    fn make_request_rejects_multipart_payload() {
        let client = reqwest::Client::new();
        let mut request = request_with("multipart/form-data", Some(json!({"file": "x"})));

        let error = make_request(&client, &mut request, &HashMap::new())
            .expect_err("multipart payloads must be rejected, not panic");

        assert!(
            error.to_string().contains("multipart/form-data"),
            "error should name the unsupported content type: {error}"
        );
    }

    #[test]
    fn make_request_builds_json_payload() {
        let client = reqwest::Client::new();
        let mut request = request_with("application/json", Some(json!({"a": 1})));

        assert!(make_request(&client, &mut request, &HashMap::new()).is_ok());
    }

    #[test]
    fn make_request_maps_every_supported_method_onto_the_wire() {
        let client = reqwest::Client::new();
        for verb in [
            "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE",
        ] {
            let mut request: TestRequest = serde_json::from_value(serde_json::json!({
                "name": "method probe",
                "url": "http://example.test/x",
                "method": verb,
            }))
            .expect(verb);

            let built = make_request(&client, &mut request, &HashMap::new())
                .expect(verb)
                .build()
                .expect(verb);
            assert_eq!(built.method().as_str(), verb);
        }
    }

    // ---------- remove_json_field / remove_header_field ----------

    #[test]
    fn removes_top_level_field() {
        let mut v = json!({"a": 1, "$id": "x"});
        remove_json_fields(&mut v, &["$id".to_string()]);
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn removes_field_recursively_in_nested_objects_and_arrays() {
        let mut v = json!({
            "$id": "1",
            "items": [
                {"$id": "2", "sku": "A"},
                {"nested": {"$id": "3", "ok": true}}
            ]
        });
        remove_json_fields(&mut v, &["$id".to_string()]);
        assert_eq!(
            v,
            json!({"items": [{"sku": "A"}, {"nested": {"ok": true}}]})
        );
    }

    #[test]
    fn removes_dotted_path_field() {
        let mut v = json!({"parent": {"child": 1, "keep": 2}});
        remove_json_fields(&mut v, &["parent.child".to_string()]);
        assert_eq!(v, json!({"parent": {"keep": 2}}));
    }

    #[test]
    fn removes_headers_field_at_top_level_only() {
        let mut v = json!({"headers": {"h": 1}, "data": {"headers": {"h": 2}}});
        remove_header_field(&mut v);
        assert_eq!(v, json!({"data": {"headers": {"h": 2}}}));
    }

    // ---------- apply_json_sort ----------

    #[test]
    fn sorts_top_level_array_of_objects_by_string_field() {
        let mut v = json!([
            {"inventSiteId": "DOU"},
            {"inventSiteId": "BRO"},
            {"inventSiteId": "PEN"}
        ]);
        apply_json_sorts(&mut v, &["$.inventSiteId".to_string()]);
        assert_eq!(
            v,
            json!([
                {"inventSiteId": "BRO"},
                {"inventSiteId": "DOU"},
                {"inventSiteId": "PEN"}
            ])
        );
    }

    #[test]
    fn sorts_nested_array_by_numeric_field() {
        let mut v = json!({"data": [{"qty": 3}, {"qty": 1}, {"qty": 2}]});
        apply_json_sorts(&mut v, &["$.qty".to_string()]);
        assert_eq!(v, json!({"data": [{"qty": 1}, {"qty": 2}, {"qty": 3}]}));
    }

    #[test]
    fn objects_missing_the_sort_field_go_last() {
        let mut v = json!([{"other": 1}, {"qty": 2}, {"qty": 1}]);
        apply_json_sorts(&mut v, &["$.qty".to_string()]);
        assert_eq!(v, json!([{"qty": 1}, {"qty": 2}, {"other": 1}]));
    }

    #[test]
    fn empty_sort_path_is_a_no_op() {
        let mut v = json!([{"b": 1}, {"a": 1}]);
        apply_json_sorts(&mut v, &["$.".to_string()]);
        assert_eq!(v, json!([{"b": 1}, {"a": 1}]));
    }

    // ---------- compare_json_values ----------

    #[test]
    fn compares_strings_numbers_and_bools() {
        use std::cmp::Ordering;

        assert_eq!(
            compare_json_values(Some(&json!("a")), Some(&json!("b"))),
            Ordering::Less
        );
        assert_eq!(
            compare_json_values(Some(&json!(2.5)), Some(&json!(1))),
            Ordering::Greater
        );
        assert_eq!(
            compare_json_values(Some(&json!(false)), Some(&json!(true))),
            Ordering::Less
        );
    }

    #[test]
    fn missing_values_sort_after_present_ones() {
        use std::cmp::Ordering;

        assert_eq!(
            compare_json_values(None, Some(&json!(1))),
            Ordering::Greater
        );
        assert_eq!(compare_json_values(Some(&json!(1)), None), Ordering::Less);
        assert_eq!(compare_json_values(None, None), Ordering::Equal);
    }
}
