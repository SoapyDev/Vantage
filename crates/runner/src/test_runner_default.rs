use crate::sequence::run_sequence;
use crate::test_runner::TestRunner;
use async_trait::async_trait;
use reqwest::Client;
use vantage_core::dictionary::Dictionary;
use vantage_core::result::{RequestType, TestResult};
use vantage_core::test_suite::{SuiteConfig, TestSuite};

/// Runs the tests against the primary environment, graded against their
/// static expectations.
#[derive(Clone, Copy, Default)]
pub struct TestRunnerDefault;

#[async_trait]
impl TestRunner for TestRunnerDefault {
    async fn run(
        &self,
        client: &Client,
        suite: &mut TestSuite,
        dictionary: &mut Dictionary,
    ) -> Result<Vec<TestResult>, anyhow::Error> {
        let config: SuiteConfig = (&mut *suite).into();
        run_sequence(
            &mut suite.tests,
            RequestType::Test,
            client,
            &config,
            dictionary,
        )
        .await
    }
}
