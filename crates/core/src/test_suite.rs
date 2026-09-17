use crate::HttpMethod;
use crate::action::Action;
use crate::load::LoadProfile;
use crate::request::TestRequest;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// A test suite deserialized from a `.json` file: the suite-level defaults
/// plus the `steps` (setup requests) and `tests` (asserted requests) to run.
///
/// Suite-level `url`, `method`, `headers`, `sorts`, and `ignored_fields` are
/// inherited by each request unless the request overrides them (see
/// [`SuiteConfig`] and [`TestRequest::update_from`](crate::request::TestRequest::update_from)).
#[derive(Debug, Deserialize)]
pub struct TestSuite {
    /// Base URL applied to requests that do not set their own.
    pub url: String,
    /// Default HTTP method for requests that do not set their own.
    #[serde(default)]
    pub method: HttpMethod,
    /// Headers merged into every request; request-level entries win on
    /// conflict.
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// JSON pointers identifying array fields to sort before comparison.
    #[serde(default)]
    pub sorts: Vec<String>,
    /// Field paths stripped from responses before comparison.
    #[serde(default)]
    pub ignored_fields: Vec<String>,
    /// Setup requests run in order before the tests; their responses can feed
    /// the dictionary but are not asserted against.
    #[serde(default)]
    pub steps: Vec<TestRequest>,
    /// The asserted requests that make up the suite.
    pub tests: Vec<TestRequest>,

    /// CLI actions executed once before the steps.
    #[serde(default)]
    pub before_all: Vec<Action>,

    /// CLI actions executed once after the tests.
    #[serde(default)]
    pub after_all: Vec<Action>,

    /// Optional open-loop load profile, used only in `--benchmark --load`
    /// mode. The CLI `--load` flag overrides this when both are given. Its
    /// per-shape rules are not checked by serde; the CLI calls
    /// [`LoadProfile::validate`](crate::load::LoadProfile::validate) before use.
    #[serde(default)]
    pub load: Option<LoadProfile>,

    /// Path the suite was loaded from; set by the loader, not deserialized.
    #[serde(skip)]
    pub file_path: Option<String>,
}

impl TestSuite {
    /// Narrows the suite to the tests whose `name` appears in `names`,
    /// preserving their original order, and returns the subset of `names`
    /// that matched at least one test here.
    ///
    /// Steps are deliberately left untouched: they prime the dictionary
    /// (e.g. an auth token) and must still run when the tests are reduced to
    /// a subset. The returned set lets a multi-suite run tell which requested
    /// names matched somewhere and which matched nowhere.
    pub fn retain_tests_named(&mut self, names: &HashSet<String>) -> HashSet<String> {
        let mut matched = HashSet::new();
        self.tests.retain(|test| {
            if names.contains(&test.name) {
                matched.insert(test.name.clone());
                true
            } else {
                false
            }
        });
        matched
    }
}

/// Suite-level defaults extracted from a [`TestSuite`] and applied to each
/// request before execution.
pub struct SuiteConfig {
    /// Base URL inherited by requests without their own `url`.
    pub url: Option<String>,
    /// Default HTTP method.
    pub method: HttpMethod,
    /// Headers merged into every request (request-level entries win).
    pub headers: HashMap<String, String>,
    /// JSON pointers of array fields to sort before comparison.
    pub sorts: Vec<String>,
    /// Field paths stripped from responses before comparison.
    pub ignored_fields: Vec<String>,
}

impl From<&mut TestSuite> for SuiteConfig {
    fn from(suite: &mut TestSuite) -> Self {
        Self {
            url: Some(suite.url.clone()),
            method: suite.method,
            headers: suite.headers.clone(),
            sorts: suite.sorts.clone(),
            ignored_fields: suite.ignored_fields.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// Builds a suite with one step and the given test names.
    fn suite_with(step_names: &[&str], test_names: &[&str]) -> TestSuite {
        let req = |name: &str| serde_json::json!({ "name": name, "payload": null, "expected_response": null });
        let json = serde_json::json!({
            "url": "http://x",
            "steps": step_names.iter().map(|n| req(n)).collect::<Vec<_>>(),
            "tests": test_names.iter().map(|n| req(n)).collect::<Vec<_>>(),
        });
        serde_json::from_value(json).expect("valid suite fixture")
    }

    #[test]
    fn keeps_only_the_named_tests_in_original_order() {
        let mut suite = suite_with(&[], &["a", "b", "c", "d"]);
        suite.retain_tests_named(&names(&["c", "a"]));
        let kept: Vec<&str> = suite.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            kept,
            vec!["a", "c"],
            "order must follow the suite, not the filter"
        );
    }

    #[test]
    fn returns_the_names_that_matched() {
        let mut suite = suite_with(&[], &["a", "b", "c"]);
        let matched = suite.retain_tests_named(&names(&["a", "c", "missing"]));
        assert_eq!(matched, names(&["a", "c"]));
    }

    #[test]
    fn a_name_matching_nothing_yields_an_empty_match_set() {
        let mut suite = suite_with(&[], &["a", "b"]);
        let matched = suite.retain_tests_named(&names(&["zzz"]));
        assert!(matched.is_empty());
        assert!(
            suite.tests.is_empty(),
            "no test should survive an all-miss filter"
        );
    }

    #[test]
    fn steps_are_never_filtered() {
        let mut suite = suite_with(&["auth"], &["a", "b"]);
        suite.retain_tests_named(&names(&["a"]));
        let steps: Vec<&str> = suite.steps.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            steps,
            vec!["auth"],
            "steps prime the dictionary and must survive"
        );
    }

    #[test]
    fn a_suite_without_a_load_block_has_no_profile() {
        let suite = suite_with(&[], &["a"]);
        assert!(suite.load.is_none());
    }

    #[test]
    fn a_suite_load_block_deserializes_into_a_profile() {
        let json = serde_json::json!({
            "url": "http://x",
            "tests": [{ "name": "a", "payload": null, "expected_response": null }],
            "load": {
                "stages": [
                    { "shape": "ramp", "duration": "3m", "target_cps": 30 },
                    { "shape": "hold", "duration": "2m" }
                ]
            }
        });
        let suite: TestSuite = serde_json::from_value(json).expect("valid suite with load");
        let load = suite.load.expect("the load block must deserialize");
        load.validate().expect("the declared profile is valid");
        assert_eq!(load, LoadProfile::parse("ramp:3m:30,hold:2m").unwrap());
    }
}
