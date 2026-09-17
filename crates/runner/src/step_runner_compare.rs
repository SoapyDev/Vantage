use crate::sequence::run_sequence_compare;
use crate::step_runner::StepRunner;
use async_trait::async_trait;
use reqwest::Client;
use vantage_core::dictionary::Dictionary;
use vantage_core::result::{RequestType, TestResult};
use vantage_core::test_suite::{SuiteConfig, TestSuite};

/// Runs each step against both environments concurrently (compare mode).
#[derive(Clone, Copy, Default)]
pub struct StepRunnerCompare;

#[async_trait]
impl StepRunner for StepRunnerCompare {
    async fn run(
        &self,
        client: &Client,
        suite: &mut TestSuite,
        dictionary: &mut Dictionary,
    ) -> Result<Vec<TestResult>, anyhow::Error> {
        let config: SuiteConfig = (&mut *suite).into();
        run_sequence_compare(
            &mut suite.steps,
            RequestType::Step,
            client,
            &config,
            dictionary,
        )
        .await
    }
}
