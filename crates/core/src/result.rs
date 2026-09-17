use crate::request::TestRequest;
use serde_json::Value;
use std::collections::HashMap;

/// What produced a [`TestResult`]: a setup `Step`, an asserted `Test`, or a
/// CLI `Action` hook. Drives how the result is numbered and displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RequestType {
    /// A setup request (suite `steps`), not asserted against.
    Step,
    /// An asserted request (suite `tests`).
    Test,
    /// A CLI action hook attached to a step, test, or the suite.
    Action,
}

/// Outcome of a single executed request (or action), holding both what was
/// sent and what came back, the expected vs. actual comparison inputs, and the
/// pass/fail verdict. Serialized into the reports.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TestResult {
    /// Display name of the request or action.
    pub name: String,
    /// Headers sent with the request (template form). Serialized redacted.
    pub request_headers: HashMap<String, String>,
    /// Headers received in the response. Serialized redacted.
    pub headers: HashMap<String, String>,
    /// Request payload actually sent (templates resolved).
    pub payload: Option<Value>,
    /// HTTP status received, if the request completed.
    pub status: Option<u16>,
    /// Response body after sorting and field stripping.
    pub body: Option<Value>,
    /// Status the request was expected to return.
    pub expected_status: u16,
    /// Body the response was expected to match, when asserted.
    pub expected_body: Option<Value>,
    /// Wall-clock duration of the request, in milliseconds.
    pub duration: u128,
    /// Error message when the request could not be completed.
    pub error: Option<String>,
    /// Whether the request met its expectations.
    pub is_success: bool,
    /// What kind of request this result came from.
    pub request_type: RequestType,
    /// Originating test-suite label (file stem). Set only for grouped runs so
    /// the aggregated report can section results by suite. Omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
}

impl Default for TestResult {
    fn default() -> Self {
        Self {
            name: String::new(),
            request_headers: HashMap::new(),
            headers: HashMap::new(),
            payload: None,
            status: None,
            body: None,
            expected_status: 200,
            expected_body: None,
            duration: 0,
            error: None,
            is_success: false,
            request_type: RequestType::Test,
            suite: None,
        }
    }
}

/// Builder API: [`TestResult::new`] starts from the defaults and the `with_*`
/// methods set individual fields, each consuming and returning `self`.
impl TestResult {
    /// Creates a result of the given kind with every other field defaulted.
    #[must_use]
    pub fn new(request_type: RequestType) -> Self {
        Self {
            request_type,
            ..Default::default()
        }
    }

    /// Sets the [`RequestType`].
    #[must_use]
    pub const fn with_request_type(mut self, request_type: RequestType) -> Self {
        self.request_type = request_type;
        self
    }
    /// Sets the display name.
    #[must_use]
    pub fn with_name(mut self, name: String) -> Self {
        self.name = name;
        self
    }
    /// Sets the received HTTP status.
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// Sets the response body.
    #[must_use]
    pub fn with_body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Sets the sent payload.
    #[must_use]
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = Some(payload);
        self
    }

    /// Sets the response headers.
    #[must_use]
    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    /// Sets the expected HTTP status.
    #[must_use]
    pub const fn with_expected_status(mut self, status: u16) -> Self {
        self.expected_status = status;
        self
    }

    /// Sets the expected response body.
    #[must_use]
    pub fn with_expected_body(mut self, body: Value) -> Self {
        self.expected_body = Some(body);
        self
    }

    /// Sets the measured duration, in milliseconds.
    #[must_use]
    pub const fn with_duration(mut self, duration: u128) -> Self {
        self.duration = duration;
        self
    }
    /// Sets the error message.
    #[must_use]
    pub fn with_error(mut self, error: String) -> Self {
        self.error = Some(error);
        self
    }
    /// Sets the pass/fail verdict.
    #[must_use]
    pub const fn with_success(mut self, is_success: bool) -> Self {
        self.is_success = is_success;
        self
    }
}

impl From<&TestRequest> for TestResult {
    fn from(request: &TestRequest) -> Self {
        Self {
            name: request.name.clone(),
            request_headers: request.headers.clone(),
            payload: request.payload.clone(),
            expected_status: request.expected_status,
            expected_body: request.expected_response.clone(),
            ..Default::default()
        }
    }
}
