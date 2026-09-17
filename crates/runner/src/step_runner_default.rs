use crate::sequence::run_sequence;
use crate::step_runner::StepRunner;
use anyhow::Error;
use async_trait::async_trait;
use reqwest::Client;
use vantage_core::dictionary::Dictionary;
use vantage_core::result::{RequestType, TestResult};
use vantage_core::test_suite::{SuiteConfig, TestSuite};

/// Runs the steps against the primary environment only.
#[derive(Clone, Copy, Default)]
pub struct StepRunnerDefault;

#[async_trait]
impl StepRunner for StepRunnerDefault {
    async fn run(
        &self,
        client: &Client,
        suite: &mut TestSuite,
        dictionary: &mut Dictionary,
    ) -> Result<Vec<TestResult>, Error> {
        let config: SuiteConfig = (&mut *suite).into();
        run_sequence(
            &mut suite.steps,
            RequestType::Step,
            client,
            &config,
            dictionary,
        )
        .await
    }
}
