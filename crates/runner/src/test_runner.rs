use async_trait::async_trait;
use reqwest::Client;
use vantage_core::dictionary::Dictionary;
use vantage_core::result::TestResult;
use vantage_core::test_suite::TestSuite;

/// Strategy for running a suite's asserted `tests` (default or compare
/// mode).
#[async_trait]
pub trait TestRunner {
    /// Runs the suite's tests in order, returning one result per executed
    /// request (hook actions included).
    ///
    /// # Errors
    ///
    /// Returns an error when the whole run must stop (e.g. an `abort` hook);
    /// per-request failures are reported in the results instead.
    async fn run(
        &self,
        client: &Client,
        suite: &mut TestSuite,
        dictionary: &mut Dictionary,
    ) -> Result<Vec<TestResult>, anyhow::Error>;
}
