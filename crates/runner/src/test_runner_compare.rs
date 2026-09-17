use crate::sequence::run_sequence_compare;
use crate::test_runner::TestRunner;
use async_trait::async_trait;
use reqwest::Client;
use vantage_core::dictionary::Dictionary;
use vantage_core::result::{RequestType, TestResult};
use vantage_core::test_suite::{SuiteConfig, TestSuite};

/// Runs each test against both environments concurrently and grades the
/// two responses against each other (compare mode).
#[derive(Clone, Copy, Default)]
pub struct TestRunnerCompare;

#[async_trait]
impl TestRunner for TestRunnerCompare {
    async fn run(
        &self,
        client: &Client,
        suite: &mut TestSuite,
        dictionary: &mut Dictionary,
    ) -> Result<Vec<TestResult>, anyhow::Error> {
        let config: SuiteConfig = (&mut *suite).into();
        run_sequence_compare(
            &mut suite.tests,
            RequestType::Test,
            client,
            &config,
            dictionary,
        )
        .await
    }
}
