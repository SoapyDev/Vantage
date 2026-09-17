use std::collections::HashMap;

use crate::request::TestRequest;
use serde_json::Value;

use crate::template_engine::injector::Injector;

/// A `capture` entry whose JSON pointer did not resolve against a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureMiss {
    /// The variable the capture would have populated.
    pub name: String,
    /// The JSON pointer that was not found in the response body.
    pub pointer: String,
}

/// Applies a request's `capture` map to a response body, feeding the
/// dictionary for downstream requests.
pub struct Extractor;

impl Extractor {
    /// Applies a request's `capture` map to a response body, inserting each
    /// resolved value into `dictionary`. Returns the entries whose pointer was
    /// not found in the body, so the caller can warn about them.
    pub fn extract(
        body: &Value,
        request: &TestRequest,
        dictionary: &mut HashMap<String, Value>,
    ) -> Vec<CaptureMiss> {
        let mut misses = Vec::new();
        for (name, path) in &request.capture {
            match body.pointer(path) {
                Some(val) => {
                    // Capture keys are templates: `{{sku}}-detail` resolves
                    // against the current dictionary before insertion.
                    // Unresolvable keys are kept literal.
                    let key =
                        Injector::inject_str(name, dictionary).unwrap_or_else(|| name.clone());
                    dictionary.insert(key, val.clone());
                }
                None => misses.push(CaptureMiss {
                    name: name.clone(),
                    pointer: path.clone(),
                }),
            }
        }
        misses
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request_with_capture(pairs: &[(&str, &str)]) -> TestRequest {
        let capture: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        serde_json::from_value(json!({
            "name": "t",
            "capture": capture
        }))
        .unwrap()
    }

    // ---------- Regression ----------

    #[test]
    fn extracts_value_by_pointer() {
        let req = request_with_capture(&[("access_token", "/access_token")]);
        let body = json!({"access_token": "tok-123"});
        let mut dict = HashMap::new();
        Extractor::extract(&body, &req, &mut dict);
        assert_eq!(dict.get("access_token"), Some(&json!("tok-123")));
    }

    #[test]
    fn empty_pointer_extracts_whole_body() {
        let req = request_with_capture(&[("detail", "")]);
        let body = json!({"price": 19.07});
        let mut dict = HashMap::new();
        Extractor::extract(&body, &req, &mut dict);
        assert_eq!(dict.get("detail"), Some(&json!({"price": 19.07})));
    }

    #[test]
    fn missing_pointer_inserts_nothing_and_is_reported() {
        let req = request_with_capture(&[("x", "/nope")]);
        let body = json!({"a": 1});
        let mut dict = HashMap::new();
        let misses = Extractor::extract(&body, &req, &mut dict);
        assert!(dict.is_empty());
        assert_eq!(
            misses,
            vec![CaptureMiss {
                name: "x".to_string(),
                pointer: "/nope".to_string()
            }]
        );
    }

    // ---------- New: dynamic extract keys ----------

    #[test]
    fn extract_key_is_injected_from_dictionary() {
        let req = request_with_capture(&[("{{sku}}-detail", "")]);
        let body = json!({"price": 19.07});
        let mut dict = HashMap::new();
        dict.insert("sku".to_string(), json!("100393-501"));
        Extractor::extract(&body, &req, &mut dict);
        assert_eq!(
            dict.get("100393-501-detail"),
            Some(&json!({"price": 19.07}))
        );
    }

    #[test]
    fn extract_key_with_unresolvable_template_is_kept_literal() {
        let req = request_with_capture(&[("{{missing}}-detail", "")]);
        let body = json!({"a": 1});
        let mut dict = HashMap::new();
        Extractor::extract(&body, &req, &mut dict);
        assert_eq!(dict.get("{{missing}}-detail"), Some(&json!({"a": 1})));
    }
}
