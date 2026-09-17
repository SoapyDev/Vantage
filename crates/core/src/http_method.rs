use serde::{Deserialize, Serialize};

/// HTTP method supported by a request. Deserialized from the upper-case verb
/// (`"GET"`, `"POST"`, ...) and defaults to [`HttpMethod::Post`].
///
/// Every standard method is supported except `CONNECT`, which establishes a
/// proxy tunnel rather than addressing an endpoint and cannot be sent
/// through the HTTP client anyway.
#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// HTTP `GET`.
    #[serde(rename = "GET")]
    Get,
    /// HTTP `HEAD`.
    #[serde(rename = "HEAD")]
    Head,
    /// HTTP `POST` (the default when no method is specified).
    #[default]
    #[serde(rename = "POST")]
    Post,
    /// HTTP `PUT`.
    #[serde(rename = "PUT")]
    Put,
    /// HTTP `PATCH`.
    #[serde(rename = "PATCH")]
    Patch,
    /// HTTP `DELETE`.
    #[serde(rename = "DELETE")]
    Delete,
    /// HTTP `OPTIONS`.
    #[serde(rename = "OPTIONS")]
    Options,
    /// HTTP `TRACE`.
    #[serde(rename = "TRACE")]
    Trace,
}

impl HttpMethod {
    /// The upper-case HTTP verb (`"GET"`, `"POST"`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
            Self::Trace => "TRACE",
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Every supported verb, paired with its wire form.
    const VERBS: &[(HttpMethod, &str)] = &[
        (HttpMethod::Get, "GET"),
        (HttpMethod::Head, "HEAD"),
        (HttpMethod::Post, "POST"),
        (HttpMethod::Put, "PUT"),
        (HttpMethod::Patch, "PATCH"),
        (HttpMethod::Delete, "DELETE"),
        (HttpMethod::Options, "OPTIONS"),
        (HttpMethod::Trace, "TRACE"),
    ];

    #[test]
    fn every_verb_deserializes_from_its_upper_case_form() {
        for (method, verb) in VERBS {
            let parsed: HttpMethod = serde_json::from_value(serde_json::json!(verb)).expect(verb);
            assert_eq!(parsed, *method, "{verb}");
        }
    }

    #[test]
    fn as_str_round_trips_with_serde() {
        for (method, verb) in VERBS {
            assert_eq!(method.as_str(), *verb);
            assert_eq!(serde_json::json!(method), serde_json::json!(verb));
        }
    }

    #[test]
    fn default_method_is_post() {
        assert_eq!(HttpMethod::default(), HttpMethod::Post);
    }

    #[test]
    fn lower_case_and_unknown_verbs_are_rejected() {
        for bad in ["get", "FETCH", "CONNECT", ""] {
            let outcome: Result<HttpMethod, _> = serde_json::from_value(serde_json::json!(bad));
            assert!(outcome.is_err(), "{bad:?} must be rejected");
        }
    }
}
