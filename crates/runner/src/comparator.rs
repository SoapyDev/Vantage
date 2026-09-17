//! Grading of executed requests against their expectations.
//!
//! [`compare_single`] grades one response against the request's static
//! expectations (`expected_status`, `expected_response`). [`compare_pair`]
//! grades compare-mode runs, where the right-hand side (the compare
//! environment) is the expectation for the left-hand side.

use assert_json_diff::assert_json_matches_no_panic;
use vantage_core::result::{RequestType, TestResult};

/// Marks `result` failed with `error`, keeping a pre-existing error message.
fn fail(result: &mut TestResult, error: String) {
    result.is_success = false;
    if result.error.is_none() {
        result.error = Some(error);
    }
}

/// Compares two bodies strictly; a mismatch (or one side missing) fails
/// `result`.
fn compare_bodies(
    result: &mut TestResult,
    body: Option<&serde_json::Value>,
    expected: Option<&serde_json::Value>,
) {
    match (body, expected) {
        (None, None) => {}
        (None, Some(expected)) => {
            fail(
                result,
                format!("Body mismatch: expected {expected:?} but got none"),
            );
        }
        (Some(body), None) => {
            fail(
                result,
                format!("Body mismatch: got {body:?} but expected none"),
            );
        }
        (Some(body), Some(expected)) => {
            let config = assert_json_diff::Config::new(assert_json_diff::CompareMode::Strict);
            if let Err(e) = assert_json_matches_no_panic(body, expected, config) {
                fail(result, e);
            }
        }
    }
}

/// Grades a single result against its own expectations.
///
/// Steps are graded on status only; tests additionally compare the received
/// body against `expected_body` (strict JSON match).
pub(crate) fn compare_single(result: TestResult) -> TestResult {
    let mut result = result;
    result.is_success = true;

    if result.status.is_none_or(|s| s != result.expected_status) {
        let message = format!(
            "Status mismatch: expected {} but got {:?}",
            result.expected_status, result.status
        );
        fail(&mut result, message);
        return result;
    }

    if result.request_type == RequestType::Test {
        let body = result.body.clone();
        let expected = result.expected_body.clone();
        compare_bodies(&mut result, body.as_ref(), expected.as_ref());
    }

    result
}

/// Grades a compare-mode pair: the right-hand side (compare environment) is
/// surfaced as the expectation of the returned result, so reports diff
/// lhs (received) against rhs (expected) rather than the suite's static
/// `expected_response`.
///
/// # Errors
///
/// Returns an error when the two results are not of the same
/// [`RequestType`].
pub(crate) fn compare_pair(lhs: TestResult, rhs: TestResult) -> Result<TestResult, anyhow::Error> {
    if lhs.request_type != rhs.request_type {
        return Err(anyhow::anyhow!(
            "Cannot compare a {:?} with a {:?}",
            lhs.request_type,
            rhs.request_type
        ));
    }

    let request_type = lhs.request_type;
    let mut result = lhs.clone();
    result.is_success = true;
    result.expected_body = rhs.body.clone();
    result.expected_status = rhs.status.unwrap_or(result.expected_status);

    match request_type {
        // Steps: both sides must meet their own expected status.
        RequestType::Step | RequestType::Action => {
            if lhs.status.is_none_or(|s| s != lhs.expected_status)
                || rhs.status.is_none_or(|s| s != rhs.expected_status)
            {
                fail(
                    &mut result,
                    format!(
                        "Status mismatch: lhs expected {} but got {:?}, rhs expected {} but got {:?}",
                        lhs.expected_status, lhs.status, rhs.expected_status, rhs.status
                    ),
                );
            }
        }
        // Tests: statuses must agree with each other and with the suite.
        RequestType::Test => {
            if lhs.status != rhs.status || lhs.status.is_some_and(|s| s != lhs.expected_status) {
                fail(
                    &mut result,
                    format!(
                        "Status mismatch: lhs : {:?}, rhs: {:?}, expected: {:?}",
                        lhs.status, rhs.status, lhs.expected_status
                    ),
                );
                return Ok(result);
            }

            compare_bodies(&mut result, lhs.body.as_ref(), rhs.body.as_ref());
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vantage_core::result::RequestType;

    fn step(status: Option<u16>, expected_status: u16) -> TestResult {
        let mut result = TestResult::new(RequestType::Step)
            .with_name("step".to_string())
            .with_expected_status(expected_status);
        result.status = status;
        result
    }

    fn test(status: Option<u16>, expected_status: u16) -> TestResult {
        let mut result = TestResult::new(RequestType::Test)
            .with_name("test".to_string())
            .with_expected_status(expected_status);
        result.status = status;
        result
    }

    // ---------- guards ----------

    #[test]
    fn mixed_request_types_are_an_error() {
        assert!(compare_pair(step(Some(200), 200), test(Some(200), 200)).is_err());
    }

    // ---------- steps: status only ----------

    #[test]
    fn step_with_expected_status_passes() {
        assert!(compare_single(step(Some(200), 200)).is_success);
    }

    #[test]
    fn step_with_wrong_status_fails_with_error() {
        let result = compare_single(step(Some(500), 200));
        assert!(!result.is_success);
        assert!(result.error.as_deref().unwrap_or("").contains("500"));
    }

    #[test]
    fn step_without_status_fails() {
        assert!(!compare_single(step(None, 200)).is_success);
    }

    #[test]
    fn step_with_non_200_expected_status_passes() {
        assert!(compare_single(step(Some(500), 500)).is_success);
    }

    #[test]
    fn step_pair_passes_when_both_match_expected() {
        assert!(
            compare_pair(step(Some(200), 200), step(Some(200), 200))
                .unwrap()
                .is_success
        );
    }

    #[test]
    fn step_pair_fails_when_one_side_mismatches() {
        let result = compare_pair(step(Some(200), 200), step(Some(500), 200)).unwrap();
        assert!(!result.is_success);
    }

    // ---------- tests: status + body vs expected ----------

    #[test]
    fn test_with_wrong_status_fails_before_body_comparison() {
        let lhs = test(Some(500), 200).with_body(json!({"a": 1}));
        let result = compare_single(lhs);
        assert!(!result.is_success);
        assert!(result.error.as_deref().unwrap_or("").contains("Status"));
    }

    #[test]
    fn test_with_no_body_and_no_expected_passes() {
        assert!(compare_single(test(Some(200), 200)).is_success);
    }

    #[test]
    fn test_with_matching_body_passes() {
        let lhs = test(Some(200), 200)
            .with_body(json!({"sku": "A", "unitPrice": 1.5}))
            .with_expected_body(json!({"sku": "A", "unitPrice": 1.5}));
        let result = compare_single(lhs);
        assert!(result.is_success, "error: {:?}", result.error);
    }

    #[test]
    fn test_with_differing_body_fails() {
        let lhs = test(Some(200), 200)
            .with_body(json!({"unitPrice": 1.5}))
            .with_expected_body(json!({"unitPrice": 999.0}));
        assert!(!compare_single(lhs).is_success);
    }

    #[test]
    fn test_with_extra_field_in_body_fails_in_strict_mode() {
        let lhs = test(Some(200), 200)
            .with_body(json!({"unitPrice": 1.5, "extra": true}))
            .with_expected_body(json!({"unitPrice": 1.5}));
        assert!(!compare_single(lhs).is_success);
    }

    #[test]
    fn test_with_body_but_no_expected_fails() {
        let lhs = test(Some(200), 200).with_body(json!({"a": 1}));
        let result = compare_single(lhs);
        assert!(!result.is_success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("expected none"),
            "the message must say the body was unexpected: {:?}",
            result.error
        );
    }

    #[test]
    fn test_with_expected_but_no_body_fails() {
        let lhs = test(Some(200), 200).with_expected_body(json!({"a": 1}));
        assert!(!compare_single(lhs).is_success);
    }

    // ---------- tests: lhs vs rhs (compare mode) ----------

    #[test]
    fn test_pair_with_identical_bodies_passes() {
        let lhs = test(Some(200), 200).with_body(json!({"a": 1}));
        let rhs = test(Some(200), 200).with_body(json!({"a": 1}));
        let result = compare_pair(lhs, rhs).unwrap();
        assert!(result.is_success, "error: {:?}", result.error);
    }

    #[test]
    fn test_pair_with_differing_bodies_fails() {
        let lhs = test(Some(200), 200).with_body(json!({"a": 1}));
        let rhs = test(Some(200), 200).with_body(json!({"a": 2}));
        assert!(!compare_pair(lhs, rhs).unwrap().is_success);
    }

    #[test]
    fn test_pair_with_differing_statuses_fails() {
        let lhs = test(Some(200), 200).with_body(json!({"a": 1}));
        let rhs = test(Some(500), 200).with_body(json!({"a": 1}));
        assert!(!compare_pair(lhs, rhs).unwrap().is_success);
    }

    #[test]
    fn test_pair_with_one_missing_body_fails() {
        let lhs = test(Some(200), 200).with_body(json!({"a": 1}));
        let rhs = test(Some(200), 200);
        assert!(!compare_pair(lhs, rhs).unwrap().is_success);
    }

    // ---------- compare mode surfaces rhs as the expectation ----------

    #[test]
    fn compare_test_pair_surfaces_rhs_as_expected_body() {
        let lhs = test(Some(200), 200).with_body(json!({"env": "primary"}));
        let rhs = test(Some(200), 200).with_body(json!({"env": "compare"}));
        let result = compare_pair(lhs, rhs).unwrap();
        // Received = lhs (primary), Expected = rhs (compare environment),
        // not the suite's static expected_response.
        assert_eq!(result.body, Some(json!({"env": "primary"})));
        assert_eq!(result.expected_body, Some(json!({"env": "compare"})));
    }

    #[test]
    fn compare_test_pair_surfaces_rhs_status_as_expected_status() {
        let lhs = test(Some(200), 200).with_body(json!({"a": 1}));
        let rhs = test(Some(503), 200).with_body(json!({"a": 1}));
        let result = compare_pair(lhs, rhs).unwrap();
        assert!(!result.is_success);
        assert_eq!(result.status, Some(200));
        assert_eq!(result.expected_status, 503);
    }

    #[test]
    fn compare_step_pair_surfaces_rhs_as_expected() {
        let lhs = step(Some(200), 200).with_body(json!({"env": "primary"}));
        let rhs = step(Some(200), 200).with_body(json!({"env": "compare"}));
        let result = compare_pair(lhs, rhs).unwrap();
        assert!(result.is_success, "error: {:?}", result.error);
        assert_eq!(result.expected_body, Some(json!({"env": "compare"})));
    }
}
