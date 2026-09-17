use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A CLI command attached to a step, test, or suite, executed before or
/// after its owner. `run` and `env` values are templates resolved against
/// the current dictionary scope before execution.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Action {
    /// Display name of the action, shown in reports.
    pub name: String,

    /// Command template, e.g. `echo '{{sku}},{{result/body/unitPrice}}' >> out.csv`.
    pub run: String,

    /// Shell override: "sh", "bash", "powershell", "cmd", or a program name.
    /// Default: powershell on Windows, sh elsewhere.
    #[serde(default)]
    pub shell: Option<String>,

    /// Extra environment variables (values are templates).
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Dictionary variable receiving the trimmed stdout (JSON auto-parsed).
    #[serde(default)]
    pub capture: Option<String>,

    /// What a failure of this action does to the surrounding run.
    #[serde(default)]
    pub on_failure: OnFailure,

    /// Per-action timeout in milliseconds. Default: 10s.
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Working directory the command runs in. Default: the process's cwd.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// What an action failure (non-zero exit, timeout, spawn error) does to the
/// surrounding run.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    /// Log a WARN, keep going. Owner verdict untouched.
    #[default]
    Continue,
    /// Mark the owning step/test FAIL. As a `before` action: the request and
    /// the remaining actions of the owner are skipped.
    Fail,
    /// Stop the whole suite.
    Abort,
}
