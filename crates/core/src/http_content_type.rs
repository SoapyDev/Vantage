use serde::{Deserialize, Serialize};
use std::fmt::Display;

/// Content type of a request payload, driving how the body is encoded.
#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HttpContentType {
    /// `application/json` (the default).
    #[default]
    #[serde(rename = "application/json")]
    Json,
    /// `application/x-www-form-urlencoded`.
    #[serde(rename = "application/x-www-form-urlencoded")]
    FormURLEncoded,
    /// `multipart/form-data`. Declared for completeness; the runner rejects
    /// payloads of this type with a clear error until it is supported.
    #[serde(rename = "multipart/form-data")]
    MultipartFormData,
}

impl Display for HttpContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpContentType::Json => write!(f, "application/json"),
            HttpContentType::FormURLEncoded => write!(f, "application/x-www-form-urlencoded"),
            HttpContentType::MultipartFormData => write!(f, "multipart/form-data"),
        }
    }
}
