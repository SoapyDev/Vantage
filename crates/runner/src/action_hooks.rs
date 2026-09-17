//! Execution of `before`/`after` CLI action hooks.
//!
//! Templates in `run`, `env`, `name` and `capture` are resolved against the
//! current scope. After-hooks additionally see a `result` object
//! (`{{result/body/...}}`, `{{result/status}}`, `{{result/is_success}}`,
//! `{{result/expected/...}}`, `{{result/error}}`, `{{result/duration}}`).
//!
//! Failure semantics (`on_failure`): `continue` logs a WARN and keeps going,
//! `fail` fails the owning step/test and skips its remaining work,
//! `abort` stops the whole suite.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use vantage_actions::{ActionCommand, ActionResult, DEFAULT_TIMEOUT, execute};
use vantage_core::action::{Action, OnFailure};
use vantage_core::result::{RequestType, TestResult};
use vantage_core::template_engine::injector::Injector;

/// Which hook a list of actions belongs to; used to label result lines.
#[derive(Clone, Copy)]
pub enum HookKind {
    Before,
    After,
}

impl HookKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// What the hook decided about the rest of the owner's execution.
pub(crate) enum HookControl {
    Continue,
    /// Fail the owning step/test; skip its remaining work. Carries the error.
    FailOwner(String),
    /// Stop the whole suite. Carries the error.
    Abort(String),
}

fn inject_or_keep(template: &str, scope: &HashMap<String, Value>) -> String {
    Injector::inject_str(template, scope).unwrap_or_else(|| template.to_string())
}

/// Parses a captured stdout: JSON objects/arrays are stored structured,
/// anything else stays a trimmed string.
fn parse_capture(raw: &str) -> Value {
    if ((raw.starts_with('{') && raw.ends_with('}'))
        || (raw.starts_with('[') && raw.ends_with(']')))
        && let Ok(parsed) = serde_json::from_str::<Value>(raw)
    {
        return parsed;
    }
    Value::String(raw.to_string())
}

/// Builds the `result` object exposed to after-hooks.
pub(crate) fn result_value(result: &TestResult) -> Value {
    json!({
        "name": result.name,
        "body": result.body.clone().unwrap_or(Value::Null),
        "status": result.status,
        "is_success": result.is_success,
        "expected": result.expected_body.clone().unwrap_or(Value::Null),
        "expected_status": result.expected_status,
        "error": result.error.clone(),
        "duration": result.duration as u64,
    })
}

/// Builds the fully-resolved command for one action against `scope`.
fn build_command(action: &Action, name: &str, scope: &HashMap<String, Value>) -> ActionCommand {
    ActionCommand {
        name: name.to_string(),
        command: inject_or_keep(&action.run, scope),
        shell: action.shell.clone(),
        env: action
            .env
            .iter()
            .map(|(k, v)| (k.clone(), inject_or_keep(v, scope)))
            .collect(),
        timeout: action
            .timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis),
        cwd: action.cwd.clone(),
    }
}

/// A human-readable reason for a failed action outcome.
fn failure_detail(outcome: &ActionResult, timeout: Duration) -> String {
    if outcome.timed_out {
        format!("timed out after {}ms", timeout.as_millis())
    } else if let Some(spawn_error) = &outcome.spawn_error {
        spawn_error.clone()
    } else {
        format!(
            "exit code {:?}: {}",
            outcome.exit_code,
            outcome.stderr.trim()
        )
    }
}

/// Runs one action against `scope` (inserting its capture on success) and
/// returns its result line plus the control verdict for the owner.
async fn run_action(
    action: &Action,
    scope: &mut HashMap<String, Value>,
    kind: HookKind,
) -> (TestResult, HookControl) {
    let name = inject_or_keep(&action.name, scope);
    let command = build_command(action, &name, scope);
    let outcome = execute(&command).await;
    let success = outcome.is_success();

    if success && let Some(variable) = &action.capture {
        let key = inject_or_keep(variable, scope);
        scope.insert(key, parse_capture(outcome.stdout.trim()));
    }

    let mut result = TestResult::new(RequestType::Action)
        .with_name(format!("{}: {name}", kind.label()))
        .with_duration(outcome.duration_ms)
        .with_success(success);

    if success {
        return (result, HookControl::Continue);
    }

    let detail = failure_detail(&outcome, command.timeout);
    result.error = Some(detail.clone());

    let control = match action.on_failure {
        OnFailure::Continue => HookControl::Continue,
        OnFailure::Fail => {
            HookControl::FailOwner(format!("{} action '{name}' failed: {detail}", kind.label()))
        }
        OnFailure::Abort => {
            HookControl::Abort(format!("action '{name}' aborted the suite: {detail}"))
        }
    };
    (result, control)
}

/// Runs a list of actions sequentially against `scope`. Captures are inserted
/// into `scope`. Stops early on a `fail`/`abort` action failure.
pub(crate) async fn run_hook(
    actions: &[Action],
    scope: &mut HashMap<String, Value>,
    kind: HookKind,
) -> (Vec<TestResult>, HookControl) {
    let mut results = vec![];

    for action in actions {
        let (result, control) = run_action(action, scope, kind).await;
        results.push(result);

        if !matches!(control, HookControl::Continue) {
            return (results, control);
        }
    }

    (results, HookControl::Continue)
}

/// Builds the failed result line for a step/test whose hook failed it.
pub(crate) fn owner_failure(name: &str, request_type: RequestType, error: String) -> TestResult {
    TestResult::new(request_type)
        .with_name(name.to_string())
        .with_error(error)
        .with_success(false)
}

/// Runs before-hooks over one or more scopes (compare mode passes both
/// dictionaries). Stops at the first `fail`/`abort` outcome: a failing
/// before-hook skips the rest of the owner's work.
pub(crate) async fn run_before_hooks(
    actions: &[Action],
    scopes: &mut [&mut HashMap<String, Value>],
) -> (Vec<TestResult>, HookControl) {
    let mut results = vec![];

    for scope in scopes.iter_mut() {
        let (mut hook_results, control) = run_hook(actions, scope, HookKind::Before).await;
        results.append(&mut hook_results);

        if !matches!(control, HookControl::Continue) {
            return (results, control);
        }
    }

    (results, HookControl::Continue)
}

/// Runs after-hooks over one or more scopes with the owner's `result` object
/// exposed. A `fail` action flips the owner's verdict in place; `abort`
/// surfaces as `Err`. Captures persist in the given scopes.
pub(crate) async fn run_after_hooks(
    actions: &[Action],
    result: &mut TestResult,
    scopes: &mut [&mut HashMap<String, Value>],
) -> Result<Vec<TestResult>, anyhow::Error> {
    let mut results = vec![];

    for scope in scopes.iter_mut() {
        scope.insert("result".to_string(), result_value(result));
        let (mut hook_results, control) = run_hook(actions, scope, HookKind::After).await;
        scope.remove("result");
        results.append(&mut hook_results);

        match control {
            HookControl::Abort(message) => anyhow::bail!(message),
            HookControl::FailOwner(message) => {
                result.is_success = false;
                if result.error.is_none() {
                    result.error = Some(message);
                }
            }
            HookControl::Continue => {}
        }
    }

    Ok(results)
}

/// Runs suite-level actions (`before_all`/`after_all`) against the main
/// dictionary. `fail` has no owning line at suite level: it stops the
/// remaining suite actions but lets the suite continue. `abort` returns Err.
pub async fn run_suite_hook(
    actions: &[Action],
    dictionary: &mut vantage_core::dictionary::Dictionary,
    kind: HookKind,
) -> Result<Vec<TestResult>, anyhow::Error> {
    let (results, control) = run_hook(actions, &mut dictionary.variables, kind).await;
    match control {
        HookControl::Abort(message) => Err(anyhow::anyhow!(message)),
        _ => Ok(results),
    }
}
