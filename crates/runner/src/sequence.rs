//! Sequential execution of a suite's requests, shared by the step and test
//! runners. The four runner types are thin wrappers that pick the request
//! list ([`TestSuite::steps`] or [`TestSuite::tests`]), the
//! [`RequestType`], and one of the two flavors here.
//!
//! [`TestSuite::steps`]: vantage_core::test_suite::TestSuite::steps
//! [`TestSuite::tests`]: vantage_core::test_suite::TestSuite::tests

use std::collections::HashMap;

use reqwest::{Client, RequestBuilder};
use serde_json::Value;
use tracing::Instrument;
use vantage_core::dictionary::Dictionary;
use vantage_core::request::TestRequest;
use vantage_core::result::{RequestType, TestResult};
use vantage_core::template_engine::extractor::{CaptureMiss, Extractor};
use vantage_core::test_suite::SuiteConfig;

use crate::action_hooks::{HookControl, owner_failure, run_after_hooks, run_before_hooks};
use crate::comparator::{compare_pair, compare_single};
use crate::for_each::{inject_meta, run_loop};
use crate::{handle_request, make_request};

/// Logs a WARN for each `capture` whose pointer was not found in a response.
/// A capture miss is a side effect, so it does not fail the request; the later
/// use of the (absent) variable is what fails, pointing back here.
fn warn_capture_misses(request_name: &str, misses: &[CaptureMiss]) {
    for miss in misses {
        tracing::warn!(
            "capture '{}' (pointer '{}') not found in response of '{}'",
            miss.name,
            miss.pointer,
            request_name
        );
    }
}

/// Appends a failure verdict: the owner's failed line first, then the hook
/// result lines that led to (or preceded) it.
fn fail_owner(
    results: &mut Vec<TestResult>,
    mut hook_results: Vec<TestResult>,
    name: &str,
    request_type: RequestType,
    message: String,
) {
    results.push(owner_failure(name, request_type, message));
    results.append(&mut hook_results);
}

/// Resolves the request's templates and builds the reqwest request, under
/// the `prepare` span.
fn prepare(
    request: &mut TestRequest,
    client: &Client,
    variables: &HashMap<String, Value>,
) -> Result<RequestBuilder, anyhow::Error> {
    tracing::info_span!("prepare").in_scope(|| {
        inject_meta(request, variables)?;
        make_request(client, request, variables)
    })
}

/// Sends the request, grades the response against the request's own
/// expectations, and extracts `capture`d values into the dictionary.
async fn send_and_grade(
    request: &TestRequest,
    builder: RequestBuilder,
    request_type: RequestType,
    dictionary: &mut Dictionary,
) -> TestResult {
    let response = handle_request(request, builder, request_type)
        .instrument(tracing::info_span!("http"))
        .await;
    let result = tracing::info_span!("grade").in_scope(|| compare_single(response));

    if let Some(body) = &result.body {
        let misses = tracing::info_span!("extract")
            .in_scope(|| Extractor::extract(body, request, &mut dictionary.variables));
        warn_capture_misses(&request.name, &misses);
    }
    result
}

/// Runs `requests` in order against the primary dictionary: hooks, template
/// injection, request, grading, extraction.
pub(crate) async fn run_sequence(
    requests: &mut [TestRequest],
    request_type: RequestType,
    client: &Client,
    config: &SuiteConfig,
    dictionary: &mut Dictionary,
) -> Result<Vec<TestResult>, anyhow::Error> {
    let mut results = vec![];

    for request in requests.iter_mut() {
        request.update_from(config);

        if request.for_each.is_some() {
            let mut loop_results =
                run_loop(request, request_type, client, config, dictionary).await?;
            results.append(&mut loop_results);
            continue;
        }

        run_one(request, request_type, client, dictionary, &mut results).await?;
    }

    Ok(results)
}

/// Executes one request's full lifecycle - before-hooks, preparation, HTTP,
/// grading/extraction, after-hooks - appending its result lines (owner line
/// first, then its actions) to `results`.
async fn run_one(
    request: &mut TestRequest,
    request_type: RequestType,
    client: &Client,
    dictionary: &mut Dictionary,
    results: &mut Vec<TestResult>,
) -> Result<(), anyhow::Error> {
    let (mut before_results, control) =
        run_before_hooks(&request.before, &mut [&mut dictionary.variables])
            .instrument(tracing::info_span!("hooks"))
            .await;
    match control {
        HookControl::Abort(message) => anyhow::bail!(message),
        HookControl::FailOwner(message) => {
            fail_owner(
                results,
                before_results,
                &request.name,
                request_type,
                message,
            );
            return Ok(());
        }
        HookControl::Continue => {}
    }

    let builder = match prepare(request, client, &dictionary.variables) {
        Ok(builder) => builder,
        Err(e) => {
            fail_owner(
                results,
                before_results,
                &request.name,
                request_type,
                e.to_string(),
            );
            return Ok(());
        }
    };

    let mut result = send_and_grade(request, builder, request_type, dictionary).await;
    let mut after_results = run_after_hooks(
        &request.after,
        &mut result,
        &mut [&mut dictionary.variables],
    )
    .instrument(tracing::info_span!("hooks"))
    .await?;

    // Owner line first, then its actions.
    results.push(result);
    results.append(&mut before_results);
    results.append(&mut after_results);
    Ok(())
}

/// Runs `requests` in compare mode: each request is executed against both
/// environments concurrently and the two responses are graded against each
/// other. Hooks run and extractions persist once per side, each against its
/// own dictionary.
pub(crate) async fn run_sequence_compare(
    requests: &mut [TestRequest],
    request_type: RequestType,
    client: &Client,
    config: &SuiteConfig,
    dictionary: &mut Dictionary,
) -> Result<Vec<TestResult>, anyhow::Error> {
    let mut results = vec![];

    for request in requests.iter_mut() {
        request.update_from(config);

        if request.for_each.is_some() {
            anyhow::bail!(
                "for_each is not supported in compare mode yet (offending request: '{}')",
                request.name
            );
        }

        run_one_compare(request, request_type, client, dictionary, &mut results).await?;
    }

    Ok(results)
}

/// Executes one request against both environments and grades the pair,
/// appending its result lines (owner line first, then its actions) to
/// `results`.
async fn run_one_compare(
    request: &mut TestRequest,
    request_type: RequestType,
    client: &Client,
    dictionary: &mut Dictionary,
    results: &mut Vec<TestResult>,
) -> Result<(), anyhow::Error> {
    // Hooks run on both sides (captures per dictionary).
    let (mut before_results, control) = run_before_hooks(
        &request.before,
        &mut [&mut dictionary.variables, &mut dictionary.compare_variables],
    )
    .instrument(tracing::info_span!("hooks"))
    .await;
    match control {
        HookControl::Abort(message) => anyhow::bail!(message),
        HookControl::FailOwner(message) => {
            fail_owner(
                results,
                before_results,
                &request.name,
                request_type,
                message,
            );
            return Ok(());
        }
        HookControl::Continue => {}
    }

    let (lhs, rhs) = match prepare_pair(request, client, dictionary) {
        Ok(pair) => pair,
        Err(e) => {
            fail_owner(
                results,
                before_results,
                &request.name,
                request_type,
                e.to_string(),
            );
            return Ok(());
        }
    };

    let mut result = send_and_grade_pair(request, lhs, rhs, request_type, dictionary).await?;
    let mut after_results = run_after_hooks(
        &request.after,
        &mut result,
        &mut [&mut dictionary.variables, &mut dictionary.compare_variables],
    )
    .instrument(tracing::info_span!("hooks"))
    .await?;

    // Owner line first, then its actions.
    results.push(result);
    results.append(&mut before_results);
    results.append(&mut after_results);
    Ok(())
}

/// One prepared side of a compare pair: the resolved request and its builder.
type PreparedSide = (TestRequest, RequestBuilder);

/// Prepares both sides of a compare run. Each side gets its own clone:
/// template injection resolves the payload in place, so sharing one request
/// would leak the primary environment's values into the compare side.
fn prepare_pair(
    request: &TestRequest,
    client: &Client,
    dictionary: &Dictionary,
) -> Result<(PreparedSide, PreparedSide), anyhow::Error> {
    tracing::info_span!("prepare").in_scope(|| {
        let mut lhs = request.clone();
        let mut rhs = request.clone();
        inject_meta(&mut lhs, &dictionary.variables)?;
        inject_meta(&mut rhs, &dictionary.compare_variables)?;

        let lhs_builder = make_request(client, &mut lhs, &dictionary.variables)?;
        let rhs_builder = make_request(client, &mut rhs, &dictionary.compare_variables)?;
        Ok(((lhs, lhs_builder), (rhs, rhs_builder)))
    })
}

/// Fires both sides concurrently, extracts each side's captures into its own
/// dictionary, and grades the pair against each other.
async fn send_and_grade_pair(
    request: &TestRequest,
    lhs: PreparedSide,
    rhs: PreparedSide,
    request_type: RequestType,
    dictionary: &mut Dictionary,
) -> Result<TestResult, anyhow::Error> {
    let (lhs_response, rhs_response) = async {
        tokio::join!(
            handle_request(&lhs.0, lhs.1, request_type),
            handle_request(&rhs.0, rhs.1, request_type)
        )
    }
    .instrument(tracing::info_span!("http"))
    .await;

    // Each side extracts into its own dictionary.
    let (lhs_misses, rhs_misses) = tracing::info_span!("extract").in_scope(|| {
        (
            extract_side(&lhs_response, request, &mut dictionary.variables),
            extract_side(&rhs_response, request, &mut dictionary.compare_variables),
        )
    });
    warn_capture_misses(&request.name, &lhs_misses);
    warn_capture_misses(&request.name, &rhs_misses);

    tracing::info_span!("grade").in_scope(|| compare_pair(lhs_response, rhs_response))
}

/// Extracts `capture`d values from one side's body into that side's
/// dictionary; no body means nothing to extract.
fn extract_side(
    response: &TestResult,
    request: &TestRequest,
    variables: &mut HashMap<String, Value>,
) -> Vec<CaptureMiss> {
    response.body.as_ref().map_or_else(Vec::new, |body| {
        Extractor::extract(body, request, variables)
    })
}
