//! Result reporting for the test runner: console and HTML outputs. The
//! latency statistics (median, percentiles, standard deviation) live in
//! [`vantage_core::stats`], shared with the runner's benchmark mode.

pub mod benchmark;
pub mod console;
pub mod html;
mod metrics_panel;
