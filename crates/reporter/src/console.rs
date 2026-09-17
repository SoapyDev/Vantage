use colored::Colorize;
use std::fmt::Write;
pub use vantage_core::logger::TestLogger;
use vantage_core::result::{RequestType, TestResult};
use vantage_core::stats::{
    calculate_median, calculate_p90, calculate_p99, calculate_standard_deviation,
};

#[derive(Default, Clone)]
pub struct ConsoleLogger {
    verbose: bool,
    message_queue: Vec<String>,
    current: usize,
    pass: usize,
    fail: usize,
    total: usize,
    durations: Vec<u128>,
    total_duration: usize,
    max_duration: usize,
    min_duration: usize,
}

impl ConsoleLogger {
    #[must_use]
    pub fn new(verbose: bool) -> Self {
        Self {
            verbose,
            max_duration: usize::MIN,
            min_duration: usize::MAX,
            ..Default::default()
        }
    }

    fn log_next(&mut self) {
        if let Some(msg) = self.message_queue.get(self.current) {
            println!("{msg}");
            self.current += 1;
        }
    }

    #[must_use]
    fn colorized_duration(&self, duration: usize) -> colored::ColoredString {
        let duration_str = duration.to_string();
        if duration > 500 {
            duration_str.red()
        } else if duration > 250 {
            duration_str.yellow()
        } else {
            duration_str.green()
        }
    }

    /// Folds a test result into the run statistics (duration spread and
    /// pass/fail counters). Only tests count: steps and actions are setup.
    fn record_test(&mut self, result: &TestResult, total: usize) {
        self.total = total;
        self.total_duration += result.duration as usize;
        self.max_duration = std::cmp::max(self.max_duration, result.duration as usize);
        self.min_duration = std::cmp::min(self.min_duration, result.duration as usize);
        self.durations.push(result.duration);
        if result.is_success {
            self.pass += 1;
        } else {
            self.fail += 1;
        }
    }

    /// The timing lines of the summary: totals, spread, and percentiles.
    fn duration_lines(&mut self) -> String {
        self.durations.sort_unstable();
        let avg = self
            .total_duration
            .checked_div(self.total)
            .unwrap_or_default();
        let std_deviation = calculate_standard_deviation(&self.durations, avg as u128);

        let mut out = format!(
            "\nTOTAL : {}ms | MIN : {}ms | MAX : {}ms",
            self.total_duration,
            self.colorized_duration(self.min_duration),
            self.colorized_duration(self.max_duration)
        );
        let _ = write!(
            out,
            "\nAVERAGE : {}ms | STANDARD DEVIATION : {}ms",
            self.colorized_duration(avg),
            self.colorized_duration(std_deviation as usize)
        );
        let _ = write!(
            out,
            "\nMEDIAN : {}ms | P90 : {}ms | P99 : {}ms",
            self.colorized_duration(calculate_median(&self.durations) as usize),
            self.colorized_duration(calculate_p90(&self.durations) as usize),
            self.colorized_duration(calculate_p99(&self.durations) as usize)
        );
        out
    }
}

/// The colored HTTP status column; empty when no response was received.
fn status_code(result: &TestResult) -> colored::ColoredString {
    result
        .status
        .map(|status| {
            if status >= 400 {
                status.to_string().red()
            } else {
                status.to_string().green()
            }
        })
        .unwrap_or_default()
}

/// The colored verdict label. Actions never FAIL on their own line:
/// fail/abort consequences are reported on the owning step/test or the suite.
fn verdict(result: &TestResult) -> colored::ColoredString {
    match (
        result.request_type == RequestType::Action,
        result.is_success,
    ) {
        (true, true) => "OK".green(),
        (true, false) => "WARN".yellow(),
        (false, true) => "PASS".green(),
        (false, false) => "FAIL".red(),
    }
}

/// The one-line row for a result. Actions are part of their owning step/test:
/// indented, no numbering, no HTTP status column.
fn format_row(result: &TestResult, position: usize, total: usize) -> String {
    if result.request_type == RequestType::Action {
        format!(
            "\t{:<116} | {} ({}ms)",
            result.name,
            verdict(result),
            result.duration
        )
    } else {
        format!(
            "{position}/{total} - {:<120} {} | {} ({}ms)",
            result.name,
            status_code(result),
            verdict(result),
            result.duration
        )
    }
}

/// The `--verbose` block: what was sent and expected, then what came back.
fn verbose_details(result: &TestResult) -> String {
    let mut msg = String::new();
    write_request_details(&mut msg, result);
    write_response_details(&mut msg, result);
    msg
}

/// What was sent (headers, body) and what was expected (status, body).
fn write_request_details(msg: &mut String, result: &TestResult) {
    if !result.request_headers.is_empty() {
        let _ = write!(
            msg,
            "\nRequest headers : {}\n",
            serde_json::to_string_pretty(&result.request_headers)
                .unwrap_or_default()
                .blue()
        );
    }

    let _ = write!(
        msg,
        "\nRequest body : {}\n",
        serde_json::to_string_pretty(&result.payload)
            .unwrap_or_default()
            .blue()
    );

    let _ = write!(
        msg,
        "\nExpected status : {}\n",
        result.expected_status.to_string().green()
    );

    if let Some(expected_body) = &result.expected_body {
        let _ = write!(
            msg,
            "\nExpected body : {}\n",
            serde_json::to_string_pretty(&expected_body)
                .unwrap_or_default()
                .green()
        );
    }
}

/// What came back: response headers and the received body (colored by
/// verdict).
fn write_response_details(msg: &mut String, result: &TestResult) {
    if !result.headers.is_empty() {
        let _ = write!(
            msg,
            "\nResponse headers : {}\n",
            serde_json::to_string_pretty(&result.headers)
                .unwrap_or_default()
                .blue()
        );
    }

    if let Some(body) = &result.body {
        let pretty_body = serde_json::to_string_pretty(&body).unwrap_or_default();
        let body = if result.is_success {
            pretty_body.green()
        } else {
            pretty_body.red()
        };

        let _ = write!(msg, "\nReceived body : {body}\n");
    }
}

impl TestLogger for ConsoleLogger {
    fn enqueue_msg(&mut self, msg: String) {
        self.message_queue.push(msg);
    }

    fn enqueue(&mut self, result: TestResult, position: usize, total: usize) {
        if result.request_type == RequestType::Test {
            self.record_test(&result, total);
        }

        let mut msg = format_row(&result, position, total);

        if self.is_verbose() && result.request_type != RequestType::Action {
            msg.push_str(&verbose_details(&result));
        }

        if let Some(error) = &result.error {
            let _ = write!(msg, "\nError : {}\n", error.red());
        }

        self.enqueue_msg(msg);
    }

    fn log_all(&mut self) {
        while self.current < self.message_queue.len() {
            self.log_next();
        }
    }

    fn is_verbose(&self) -> bool {
        self.verbose
    }

    fn summary(&mut self) {
        let mut msg = format!(
            "TOTAL : {} | PASSED : {} | FAILED : {}",
            self.total,
            self.pass.to_string().green(),
            self.fail.to_string().red()
        );

        if self.total > 0 {
            msg.push_str(&self.duration_lines());
        }

        self.enqueue_msg(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vantage_core::result::{RequestType, TestResult};

    fn action_result(success: bool) -> TestResult {
        TestResult::new(RequestType::Action)
            .with_name("after: append result".to_string())
            .with_duration(3)
            .with_success(success)
    }

    fn test_result() -> TestResult {
        TestResult::new(RequestType::Test)
            .with_name("Price matches detail - A".to_string())
            .with_status(200)
            .with_duration(95)
            .with_success(true)
    }

    #[test]
    fn actions_are_indented_and_not_numbered() {
        let mut logger = ConsoleLogger::new(false);
        logger.enqueue(action_result(true), 1, 1);

        let msg = logger.message_queue.last().unwrap();
        assert!(msg.starts_with('\t'), "expected tab indent: {msg:?}");
        assert!(
            !msg.contains("1/1"),
            "actions must not be numbered: {msg:?}"
        );
        assert!(msg.contains("OK"));
    }

    #[test]
    fn failed_actions_display_warn_and_do_not_count() {
        let mut logger = ConsoleLogger::new(false);
        logger.enqueue(action_result(false), 1, 1);

        let msg = logger.message_queue.last().unwrap();
        assert!(msg.contains("WARN"), "{msg:?}");
        assert_eq!(logger.fail, 0, "actions are excluded from test stats");
        assert_eq!(logger.pass, 0);
    }

    #[test]
    fn tests_keep_their_numbering() {
        let mut logger = ConsoleLogger::new(false);
        logger.enqueue(test_result(), 2, 4);

        let msg = logger.message_queue.last().unwrap();
        assert!(msg.starts_with("2/4 - "), "{msg:?}");
        assert!(msg.contains("PASS"));
        assert_eq!(logger.pass, 1);
    }

    #[test]
    fn summary_with_zero_tests_does_not_panic() {
        let mut logger = ConsoleLogger::new(false);
        logger.summary();

        let msg = logger.message_queue.last().unwrap();
        assert!(msg.contains("TOTAL : 0"), "{msg:?}");
    }
}
