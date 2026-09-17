//! Execution of the `for_each` looping strategy shared by the step and test
//! runners.
//!
//! Semantics:
//! - The item source (`in`) is either an inline request (executed once) or the
//!   name of a variable already in the dictionary. `items_path` is a JSON
//!   pointer to the array inside the source (empty = the source itself).
//! - Each iteration runs against an isolated clone of the dictionary: the item
//!   is bound under `as` (default `item`) and its position under `index`.
//! - The body is `sequence` -- an ordinary request sequence run through the
//!   shared [`run_sequence`], so it behaves exactly like the top-level suite
//!   (hooks, grading, `capture`, and even a nested `for_each` all work). When
//!   `sequence` is empty the host request is the body (single-call loop).
//! - Nothing leaks back to the parent dictionary except `capture`d values,
//!   which build keyed maps: `dictionary[name][key] = value`.
//! - A failing iteration is reported but does not abort the loop.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use reqwest::Client;
use serde_json::{Map, Value};
use tracing::Instrument;
use vantage_core::dictionary::Dictionary;
use vantage_core::request::{ForEach, ForEachSource, TestRequest};
use vantage_core::result::{RequestType, TestResult};
use vantage_core::template_engine::injector::Injector;
use vantage_core::test_suite::SuiteConfig;

use crate::action_hooks::owner_failure;
use crate::comparator::compare_single;
use crate::sequence::run_sequence;
use crate::{handle_request, make_request};

/// Default cap on for_each iterations when no explicit `limit` is set.
pub(crate) const DEFAULT_LOOP_LIMIT: usize = 100;

/// Injects dictionary values into the request metadata that `make_request`
/// does not cover: the display name and the expected response body.
pub(crate) fn inject_meta(
    request: &mut TestRequest,
    dictionary: &HashMap<String, Value>,
) -> Result<(), anyhow::Error> {
    // The display name stays best-effort (cosmetic): an unresolved name must
    // not fail a request on its own.
    if let Some(name) = Injector::inject_str(&request.name, dictionary) {
        request.name = name;
    }

    let request_name = request.name.clone();
    if let Some(expected) = request.expected_response.as_mut() {
        Injector::inject_checked(expected, dictionary)
            .map_err(|e| crate::unresolved_template(e, &request_name, dictionary))?;
    }

    Ok(())
}

/// The "the loop could not start" failure carried on the host's line.
fn source_failure(host_name: &str, message: impl Into<String>) -> Box<TestResult> {
    Box::new(owner_failure(host_name, RequestType::Step, message.into()))
}

/// Executes an inline `in:` request once and returns its response body. Its
/// own result line is appended to `results`, graded as a step: it is setup,
/// never an assertion, so a test host's loop does not over-count it as a
/// test.
async fn execute_source_request(
    host_name: &str,
    source: &TestRequest,
    client: &Client,
    config: &SuiteConfig,
    dictionary: &Dictionary,
    results: &mut Vec<TestResult>,
) -> Result<Value, Box<TestResult>> {
    let mut request = source.clone();
    request.update_from(config);

    let prepared = tracing::info_span!("prepare").in_scope(|| {
        inject_meta(&mut request, &dictionary.variables)?;
        make_request(client, &mut request, &dictionary.variables)
    });
    let builder = match prepared {
        Ok(builder) => builder,
        Err(e) => {
            results.push(owner_failure(
                &request.name,
                RequestType::Step,
                e.to_string(),
            ));
            return Err(source_failure(host_name, "for_each source request failed"));
        }
    };

    let response = handle_request(&request, builder, RequestType::Step)
        .instrument(tracing::info_span!("http"))
        .await;
    let result = tracing::info_span!("grade").in_scope(|| compare_single(response));

    let body = result.body.clone();
    let success = result.is_success;
    results.push(result);

    if !success {
        return Err(source_failure(host_name, "for_each source request failed"));
    }
    Ok(body.unwrap_or(Value::Null))
}

/// Resolves the list of items to iterate over. When the source is an inline
/// request, it is executed once and its result appended to `results`.
async fn resolve_items(
    host_name: &str,
    for_each: &ForEach,
    client: &Client,
    config: &SuiteConfig,
    dictionary: &Dictionary,
    results: &mut Vec<TestResult>,
) -> Result<Vec<Value>, Box<TestResult>> {
    let source: Value = match &for_each.source {
        ForEachSource::Request(request) => {
            execute_source_request(host_name, request, client, config, dictionary, results).await?
        }
        ForEachSource::Variable(variable) => match dictionary.get(variable) {
            Some(value) => value.clone(),
            None => {
                return Err(source_failure(
                    host_name,
                    format!("for_each source variable '{variable}' not found in dictionary"),
                ));
            }
        },
    };

    let items = if for_each.items_path.is_empty() {
        Some(&source)
    } else {
        source.pointer(&for_each.items_path)
    };
    let Some(Value::Array(items)) = items else {
        return Err(source_failure(
            host_name,
            format!(
                "for_each source at '{}' is not an array",
                for_each.items_path
            ),
        ));
    };

    let limit = for_each.limit.unwrap_or(DEFAULT_LOOP_LIMIT);
    Ok(items.iter().take(limit).cloned().collect())
}

/// An isolated per-iteration dictionary: the parent's variables plus the item
/// bound under `as` and its zero-based position under `index`.
fn iteration_scope(
    dictionary: &Dictionary,
    for_each: &ForEach,
    item: &Value,
    index: usize,
) -> Dictionary {
    let mut scope = Dictionary::new();
    scope.variables = dictionary.variables.clone();
    scope
        .variables
        .insert(for_each.binding.clone(), item.clone());
    scope
        .variables
        .insert("index".to_string(), Value::Number(index.into()));
    scope
}

/// Persists `capture`d values from an iteration scope into the parent
/// dictionary as keyed maps: `dictionary[name][key] = value`.
fn capture_into(for_each: &ForEach, scope: &HashMap<String, Value>, dictionary: &mut Dictionary) {
    for (name, entry) in &for_each.capture {
        let key = Injector::inject_str(&entry.key, scope).unwrap_or_else(|| entry.key.clone());

        let mut value = Value::String(entry.value.clone());
        Injector::inject(&mut value, scope);

        let slot = dictionary
            .variables
            .entry(name.clone())
            .or_insert_with(|| Value::Object(Map::new()));

        if let Value::Object(map) = slot {
            map.insert(key, value);
        }
    }
}

/// Runs a `for_each` loop attached to any request.
///
/// Each item is bound under `for_each.binding` in an isolated scope, and the
/// body (`for_each.sequence`, or the host request when empty) is run through
/// [`run_sequence`] -- the same path as the top-level suite, so hooks, grading,
/// and nested loops behave identically. `capture`d values persist into the
/// parent dictionary after every iteration; a failing iteration does not abort
/// the loop.
///
/// Returned as a boxed future because it and [`run_sequence`] are mutually
/// recursive (a loop body may itself contain a `for_each`).
pub(crate) fn run_loop<'a>(
    host: &'a TestRequest,
    request_type: RequestType,
    client: &'a Client,
    config: &'a SuiteConfig,
    dictionary: &'a mut Dictionary,
) -> Pin<Box<dyn Future<Output = Result<Vec<TestResult>, anyhow::Error>> + Send + 'a>> {
    Box::pin(async move {
        let mut results = vec![];
        let Some(for_each) = &host.for_each else {
            return Ok(results);
        };

        // Body: the explicit sequence, or the host itself (single-call loop).
        // The host clone's `for_each` is cleared so the body does not re-enter
        // this loop.
        let body: Vec<TestRequest> = if for_each.sequence.is_empty() {
            let mut host_body = host.clone();
            host_body.for_each = None;
            vec![host_body]
        } else {
            for_each.sequence.clone()
        };

        let items = match resolve_items(
            &host.name,
            for_each,
            client,
            config,
            dictionary,
            &mut results,
        )
        .await
        {
            Ok(items) => items,
            Err(result) => {
                results.push((*result).with_request_type(request_type));
                return Ok(results);
            }
        };

        for (index, item) in items.iter().enumerate() {
            let mut scope = iteration_scope(dictionary, for_each, item, index);
            let mut body = body.clone();
            let mut iteration =
                run_sequence(&mut body, request_type, client, config, &mut scope).await?;
            results.append(&mut iteration);

            capture_into(for_each, &scope.variables, dictionary);
        }

        Ok(results)
    })
}
