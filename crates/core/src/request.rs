use crate::action::Action;
use crate::{HttpContentType, HttpMethod, test_suite::SuiteConfig};
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::default_u16;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TestRequest {
    pub name: String,

    #[serde(default)]
    pub url: Option<String>,

    /// HTTP method; inherited from the suite when absent.
    #[serde(default)]
    pub method: Option<HttpMethod>,

    #[serde(default)]
    pub content_type: HttpContentType,

    #[serde(default)]
    pub headers: HashMap<String, String>,

    pub payload: Option<Value>,

    #[serde(default)]
    pub sorts: Vec<String>,

    #[serde(default)]
    pub ignored_fields: Vec<String>,

    /// Values pulled from the response into the dictionary: `name -> JSON
    /// pointer` (an empty pointer captures the whole body). Names are
    /// themselves templates, so a `{{sku}}-detail` key resolves against the
    /// current scope before insertion.
    #[serde(default)]
    pub capture: HashMap<String, String>,

    #[serde(default = "default_u16::<200>")]
    pub expected_status: u16,

    #[serde(default)]
    pub expected_response: Option<Value>,

    /// Expands this request into one execution per item of a list.
    #[serde(default)]
    pub for_each: Option<ForEach>,

    /// CLI actions executed before this request.
    #[serde(default)]
    pub before: Vec<Action>,

    /// CLI actions executed after this request (even when it fails).
    #[serde(default)]
    pub after: Vec<Action>,
}

/// Looping strategy attached to any request.
///
/// `in` is the item source: either the name of an array variable already in
/// the dictionary, or an inline request executed once whose response provides
/// the array (`items_path` points to it; empty = the whole body). Each item is
/// bound in the loop scope under `as` (default `item`), so its fields are read
/// with the ordinary pointer syntax (`{{item/sku}}`) and `index` holds the
/// zero-based position.
///
/// `sequence` is the loop body: an ordinary request sequence, the same grammar
/// as the suite itself, so it supports hooks, `capture`, and even a nested
/// `for_each`. When empty, the host request is the body (single-call loop).
///
/// Iteration scopes are isolated; only `capture` persists back to the parent
/// dictionary, as keyed maps: `name -> { key, value }` builds
/// `dictionary[name][key] = value`.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ForEach {
    /// Item source: an array variable name, or an inline request that fetches
    /// the array.
    #[serde(rename = "in")]
    pub source: ForEachSource,

    /// JSON pointer to the array within the source. Empty = the source itself.
    #[serde(default)]
    pub items_path: String,

    /// Names the current item in the loop scope: `{{<as>}}` is the whole item,
    /// `{{<as>/field}}` a field of it. Defaults to `item`.
    #[serde(rename = "as", default = "default_item_binding")]
    pub binding: String,

    /// Loop body: an ordinary request sequence (same grammar as the suite).
    /// Empty = the host request is the body (single-call loop).
    #[serde(default)]
    pub sequence: Vec<TestRequest>,

    /// Values persisted from the iteration scope into the parent dictionary as
    /// keyed maps: `name -> { key, value }` builds `dictionary[name][key] =
    /// value`.
    #[serde(default)]
    pub capture: HashMap<String, CaptureEntry>,

    /// Safety cap on the number of iterations.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Where a [`ForEach`] draws its items from.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ForEachSource {
    /// Name of an array variable already present in the dictionary.
    Variable(String),
    /// Request executed once; its response provides the array.
    Request(Box<TestRequest>),
}

impl Default for ForEachSource {
    fn default() -> Self {
        Self::Variable(String::new())
    }
}

/// One entry of a `for_each` `capture`: builds `dictionary[name][key] = value`
/// from the iteration scope. `key` and `value` are templates.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct CaptureEntry {
    pub key: String,
    pub value: String,
}

/// Default binding name for the current item inside a `for_each` scope.
fn default_item_binding() -> String {
    "item".to_string()
}

impl TestRequest {
    /// Fills the request's blanks with the suite-level defaults: `url` and
    /// `method` when absent, `headers` for keys the request does not define
    /// (request-level entries win on conflict), and `sorts`/`ignored_fields`
    /// appended without duplicates, preserving order.
    pub fn update_from(&mut self, suite: &SuiteConfig) {
        if self.url.is_none() {
            self.url.clone_from(&suite.url);
        }

        if self.method.is_none() {
            self.method = Some(suite.method);
        }

        for (key, value) in &suite.headers {
            self.headers
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }

        self.sorts.extend(suite.sorts.iter().cloned());

        for field in &suite.ignored_fields {
            if !self.ignored_fields.contains(field) {
                self.ignored_fields.push(field.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- Regression ----------

    #[test]
    fn deserializes_request_without_for_each() {
        let req: TestRequest = serde_json::from_value(json!({
            "name": "t",
            "payload": {"_sku": "A"},
            "expected_response": {"ok": true}
        }))
        .unwrap();
        assert!(req.for_each.is_none());
        assert_eq!(req.expected_status, 200);
    }

    #[test]
    fn deserializes_suite_with_multiple_tests() {
        let suite: crate::test_suite::TestSuite = serde_json::from_value(json!({
            "url": "{{BASE_URL}}/api/example",
            "tests": [
                {"name": "first", "payload": {}, "expected_response": {"ok": true}},
                {"name": "second", "payload": {}, "expected_response": {"ok": true}}
            ]
        }))
        .unwrap();
        assert!(!suite.tests.is_empty());
        assert!(suite.tests.iter().all(|t| t.for_each.is_none()));
    }

    // ---------- Suite inheritance (update_from) ----------

    fn suite_config() -> SuiteConfig {
        SuiteConfig {
            url: Some("https://suite".to_string()),
            method: crate::HttpMethod::Post,
            headers: HashMap::from([
                ("Authorization".to_string(), "suite-token".to_string()),
                ("X-Extra".to_string(), "from-suite".to_string()),
            ]),
            sorts: vec![],
            ignored_fields: vec!["$id".to_string(), "url".to_string()],
        }
    }

    #[test]
    fn request_header_overrides_suite_default() {
        let mut req: TestRequest = serde_json::from_value(json!({
            "name": "t",
            "headers": {"Authorization": "request-token"}
        }))
        .unwrap();

        req.update_from(&suite_config());

        assert_eq!(
            req.headers.get("Authorization").map(String::as_str),
            Some("request-token"),
            "a request-level header must win over the suite default"
        );
        assert_eq!(
            req.headers.get("X-Extra").map(String::as_str),
            Some("from-suite"),
            "suite headers must fill the missing ones"
        );
    }

    #[test]
    fn inherited_ignored_fields_are_deduplicated_preserving_order() {
        let mut req: TestRequest = serde_json::from_value(json!({
            "name": "t",
            "ignored_fields": ["url", "origin"]
        }))
        .unwrap();

        req.update_from(&suite_config());

        assert_eq!(
            req.ignored_fields,
            vec!["url", "origin", "$id"],
            "request fields first, suite fields appended once, order stable"
        );
    }

    // ---------- Actions ----------

    #[test]
    fn deserializes_suite_with_actions() {
        let suite: crate::test_suite::TestSuite = serde_json::from_value(json!({
            "url": "{{BASE_URL}}/api/example",
            "before_all": [{"name": "seed", "run": "echo seed"}],
            "after_all": [{"name": "cleanup", "run": "echo done"}],
            "tests": [
                {
                    "name": "priced",
                    "payload": {},
                    "before": [
                        {"name": "load price", "run": "cat price.txt", "capture": "expected_price"}
                    ],
                    "after": [
                        {"name": "save", "run": "echo saved"}
                    ]
                }
            ]
        }))
        .unwrap();
        assert_eq!(suite.before_all.len(), 1);
        assert_eq!(suite.after_all.len(), 1);
        let test = &suite.tests[0];
        assert_eq!(test.before[0].capture.as_deref(), Some("expected_price"));
        assert_eq!(test.after.len(), 1);
    }

    #[test]
    fn deserializes_actions_with_defaults() {
        use crate::action::OnFailure;

        let req: TestRequest = serde_json::from_value(json!({
            "name": "t",
            "payload": {},
            "before": [
                {"name": "init file", "run": "echo a > out.txt"}
            ],
            "after": [
                {
                    "name": "save",
                    "run": "echo '{{result/body/x}}' >> out.txt",
                    "on_failure": "fail",
                    "capture": "saved",
                    "timeout_ms": 500,
                    "shell": "bash",
                    "env": {"K": "{{v}}"},
                    "cwd": "/tmp"
                }
            ]
        }))
        .unwrap();

        assert_eq!(req.before.len(), 1);
        assert_eq!(req.before[0].on_failure, OnFailure::Continue);
        assert!(req.before[0].capture.is_none());
        assert!(req.before[0].timeout_ms.is_none());

        let after = &req.after[0];
        assert_eq!(after.on_failure, OnFailure::Fail);
        assert_eq!(after.capture.as_deref(), Some("saved"));
        assert_eq!(after.timeout_ms, Some(500));
        assert_eq!(after.shell.as_deref(), Some("bash"));
        assert_eq!(after.env.get("K").map(String::as_str), Some("{{v}}"));
        assert_eq!(after.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn deserializes_on_failure_abort() {
        use crate::action::{Action, OnFailure};

        let action: Action = serde_json::from_value(json!({
            "name": "must work",
            "run": "mkdir reports",
            "on_failure": "abort"
        }))
        .unwrap();
        assert_eq!(action.on_failure, OnFailure::Abort);
    }

    #[test]
    fn deserializes_suite_level_actions() {
        let suite: crate::test_suite::TestSuite = serde_json::from_value(json!({
            "url": "http://x",
            "tests": [],
            "before_all": [{"name": "init", "run": "echo init"}],
            "after_all": [{"name": "done", "run": "echo done"}]
        }))
        .unwrap();
        assert_eq!(suite.before_all.len(), 1);
        assert_eq!(suite.after_all.len(), 1);
    }

    // ---------- Response capture ----------

    #[test]
    fn deserializes_request_with_capture() {
        let req: TestRequest = serde_json::from_value(json!({
            "name": "auth",
            "payload": {},
            "capture": {"access_token": "/access_token"}
        }))
        .unwrap();
        assert_eq!(
            req.capture.get("access_token").map(String::as_str),
            Some("/access_token")
        );
    }

    // ---------- for_each ----------

    #[test]
    fn deserializes_for_each_variable_source() {
        let req: TestRequest = serde_json::from_value(json!({
            "name": "loop",
            "for_each": {
                "in": "products",
                "as": "product",
                "limit": 10,
                "capture": {
                    "prices": {"key": "{{product/sku}}", "value": "{{result/body/price}}"}
                }
            },
            "payload": {"_sku": "{{product/sku}}"}
        }))
        .unwrap();

        let fe = req.for_each.unwrap();
        assert!(matches!(fe.source, ForEachSource::Variable(ref v) if v == "products"));
        assert_eq!(fe.binding, "product");
        assert_eq!(fe.limit, Some(10));
        assert!(fe.sequence.is_empty(), "empty sequence = single-call loop");
        assert!(fe.capture.contains_key("prices"));
    }

    #[test]
    fn for_each_binding_defaults_to_item() {
        let req: TestRequest = serde_json::from_value(json!({
            "name": "loop",
            "for_each": {"in": "products"},
            "payload": {"_sku": "{{item/sku}}"}
        }))
        .unwrap();
        let fe = req.for_each.unwrap();
        assert_eq!(fe.binding, "item", "the item binding defaults to `item`");
        assert!(matches!(fe.source, ForEachSource::Variable(ref v) if v == "products"));
    }

    #[test]
    fn deserializes_for_each_request_source_with_sequence() {
        let req: TestRequest = serde_json::from_value(json!({
            "name": "hydrate",
            "for_each": {
                "in": {
                    "name": "list",
                    "url": "{{BASE_URL}}/products",
                    "method": "POST",
                    "payload": {}
                },
                "items_path": "/products",
                "as": "product",
                "sequence": [
                    {
                        "name": "detail {{product/sku}}",
                        "url": "{{BASE_URL}}/detail",
                        "payload": {"_sku": "{{product/sku}}"},
                        "capture": {"detail": ""}
                    }
                ],
                "capture": {
                    "sku_details": {"key": "{{product/sku}}", "value": "{{detail}}"}
                }
            }
        }))
        .unwrap();

        let fe = req.for_each.unwrap();
        assert!(matches!(fe.source, ForEachSource::Request(_)));
        assert_eq!(fe.items_path, "/products");
        assert_eq!(fe.sequence.len(), 1);
        assert_eq!(
            fe.sequence[0].capture.get("detail").map(String::as_str),
            Some("")
        );
        let entry = fe.capture.get("sku_details").unwrap();
        assert_eq!(entry.key, "{{product/sku}}");
        assert_eq!(entry.value, "{{detail}}");
    }
}
