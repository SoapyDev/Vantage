//! Domain model shared across the workspace: the test-suite types
//! ([`test_suite`], [`request`], [`action`]), HTTP value types
//! ([`http_method`], [`http_content_type`]), execution context
//! ([`environments`], [`dictionary`]), the [`template_engine`], the
//! benchmark [`load`] profiles, and the [`result`] produced by a run.
//!
//! This crate is deliberately free of any HTTP or I/O dependency: the binding
//! to `reqwest` and the execution logic live in the `runner` crate, and reading
//! the environments configuration file lives in the `cli` crate.

pub mod action;
pub mod benchmark;
pub mod dictionary;
pub mod environments;
pub mod http_content_type;
pub mod http_method;
pub mod load;
pub mod logger;
pub mod request;
pub mod result;
pub mod stats;
pub mod template_engine;
pub mod test_suite;

pub use environments::*;
pub use http_content_type::*;
pub use http_method::*;
