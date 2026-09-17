use crate::cli_args::CliArgs;
use crate::constant::{RUN_STEP, RUN_SUITE, RUN_TEST, SUMMARY};
use crate::metrics::StepTimings;
use runner::action_hooks::HookKind;
use runner::step_runner::StepRunner;
use runner::test_runner::TestRunner;
use std::sync::{Arc, Mutex};
use vantage_core::action::Action;
use vantage_core::dictionary::Dictionary;
use vantage_core::logger::TestLogger;
use vantage_core::result::{RequestType, TestResult};
use vantage_core::test_suite::TestSuite;

pub struct TestExecutor {
    pub dictionary: Dictionary,
    pub environment: String,
    pub compare_environment: Option<String>,
    pub client: reqwest::Client,

    pub logger: Arc<Mutex<Box<dyn TestLogger + Send>>>,
    pub step_runner: Box<dyn StepRunner + Send + Sync>,
    pub test_runner: Box<dyn TestRunner + Send + Sync>,

    /// When true the run is part of a group: results are stamped with their
    /// suite label and the executor does not finalize the shared report.
    group_mode: bool,
    /// When true this executor owns its logger and writes/summarizes on
    /// completion. Grouped runs leave finalization to the caller.
    finalize: bool,
    /// Per-step timings embedded into the report when this executor finalizes
    /// (single-suite `--reports`). Grouped runs pass `None`; the caller sets
    /// the metrics on the shared report instead.
    metrics: Option<Arc<StepTimings>>,
}

impl TestExecutor {
    pub fn new(args: &CliArgs, dictionary: &Dictionary, metrics: Option<Arc<StepTimings>>) -> Self {
        let logger: Arc<Mutex<Box<dyn TestLogger + Send>>> = Arc::new(Mutex::new(args.into()));
        Self::build(args, dictionary, logger, false, true, metrics)
    }

    /// Builds an executor that feeds a shared, group-level logger. Results are
    /// tagged with their suite; the caller finalizes the report once all
    /// suites have run.
    pub fn new_grouped(
        args: &CliArgs,
        dictionary: &Dictionary,
        logger: Arc<Mutex<Box<dyn TestLogger + Send>>>,
    ) -> Self {
        Self::build(args, dictionary, logger, true, false, None)
    }

    fn build(
        args: &CliArgs,
        dictionary: &Dictionary,
        logger: Arc<Mutex<Box<dyn TestLogger + Send>>>,
        group_mode: bool,
        finalize: bool,
        metrics: Option<Arc<StepTimings>>,
    ) -> Self {
        let client = reqwest::Client::new();
        let step_runner = args.into();
        let test_runner = args.into();

        Self {
            test_runner,
            step_runner,
            dictionary: dictionary.clone(),
            environment: args.environment.clone(),
            compare_environment: args.compare_with.clone(),
            client,
            logger,
            group_mode,
            finalize,
            metrics,
        }
    }

    pub fn enqueue_results(&self, results: Vec<TestResult>) {
        if let Ok(logger) = self.logger.lock().as_mut() {
            // Actions are not numbered: they are displayed indented under
            // their owning step/test and excluded from position/total.
            let total = results
                .iter()
                .filter(|r| r.request_type != RequestType::Action)
                .count();
            let mut position = 0;
            for result in results {
                if result.request_type != RequestType::Action {
                    position += 1;
                }
                logger.enqueue(result, position, total);
            }
        }
    }

    pub fn enqueue_msg(&self, msg: String) {
        if let Ok(logger) = self.logger.lock().as_mut() {
            logger.enqueue_msg(msg);
        }
    }

    pub fn log_all(&self) {
        if let Ok(logger) = self.logger.lock().as_mut() {
            logger.log_all();
        }
    }

    pub fn summary(&self) {
        if let Ok(logger) = self.logger.lock().as_mut() {
            logger.summary();
        }
    }

    /// Stamps each result with its suite label so a shared group report can
    /// section results by suite. No-op outside group mode.
    fn stamp(mut results: Vec<TestResult>, label: &Option<String>) -> Vec<TestResult> {
        if let Some(label) = label {
            for result in &mut results {
                result.suite = Some(label.clone());
            }
        }
        results
    }

    /// Announces the run and hands the logger the suite identity.
    fn announce(&self, test_suite: &TestSuite) {
        self.enqueue_msg(RUN_SUITE.to_string());
        self.enqueue_msg(format!("Environment : {}", self.environment));
        if let Some(env) = &self.compare_environment {
            self.enqueue_msg(format!("Compare with : {env}"));
        }
        self.enqueue_msg(format!("Endpoint : {}", test_suite.url));

        if let Ok(logger) = self.logger.lock().as_mut() {
            let name = test_suite
                .file_path
                .clone()
                .unwrap_or_else(|| test_suite.url.clone());
            logger.set_suite(&name);
            logger.set_environments(&self.environment, self.compare_environment.as_deref());
        }
    }

    /// The label stamped on results in group mode (the suite's file stem), so
    /// the aggregated report can roll up and section by suite. `None` outside
    /// group mode.
    fn suite_label(&self, test_suite: &TestSuite) -> Option<String> {
        self.group_mode.then(|| {
            test_suite
                .file_path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).file_stem().and_then(|s| s.to_str()))
                .unwrap_or("suite")
                .to_string()
        })
    }

    /// Runs a suite-level hook list (`before_all`/`after_all`) when present.
    async fn run_suite_hooks(
        &self,
        actions: &[Action],
        kind: HookKind,
        title: &str,
        dictionary: &mut Dictionary,
        suite_label: &Option<String>,
    ) -> Result<(), anyhow::Error> {
        if actions.is_empty() {
            return Ok(());
        }
        self.enqueue_msg(title.to_string());
        let results = runner::action_hooks::run_suite_hook(actions, dictionary, kind).await?;
        self.enqueue_results(Self::stamp(results, suite_label));
        Ok(())
    }

    /// Runs the suite's setup steps when present.
    async fn run_steps(
        &self,
        test_suite: &mut TestSuite,
        dictionary: &mut Dictionary,
        suite_label: &Option<String>,
    ) -> Result<(), anyhow::Error> {
        if test_suite.steps.is_empty() {
            return Ok(());
        }
        self.enqueue_msg(RUN_STEP.to_string());
        let results = self
            .step_runner
            .run(&self.client, test_suite, dictionary)
            .await?;
        self.enqueue_results(Self::stamp(results, suite_label));
        Ok(())
    }

    /// Runs the suite's asserted tests when present.
    async fn run_tests(
        &self,
        test_suite: &mut TestSuite,
        dictionary: &mut Dictionary,
        suite_label: &Option<String>,
    ) -> Result<(), anyhow::Error> {
        if test_suite.tests.is_empty() {
            return Ok(());
        }
        self.enqueue_msg(RUN_TEST.to_string());
        let results = self
            .test_runner
            .run(&self.client, test_suite, dictionary)
            .await?;
        self.enqueue_results(Self::stamp(results, suite_label));
        Ok(())
    }

    /// Writes/summarizes the report for runs that own their logger. Grouped
    /// runs defer summary/report writing to the caller, which finalizes the
    /// single shared report after every suite has run.
    fn finalize_report(&self) {
        if !self.finalize {
            return;
        }
        tracing::info_span!("report").in_scope(|| {
            if let Some(timings) = &self.metrics
                && let Ok(mut logger) = self.logger.lock()
            {
                logger.set_metrics(timings.as_json());
            }
            self.summary();
            self.log_all();
        });
    }

    pub async fn execute(&self, mut test_suite: TestSuite) -> Result<(), anyhow::Error> {
        self.announce(&test_suite);
        let suite_label = self.suite_label(&test_suite);
        let mut dictionary = self.dictionary.clone();

        self.run_suite_hooks(
            &test_suite.before_all,
            HookKind::Before,
            "Before all",
            &mut dictionary,
            &suite_label,
        )
        .await?;

        self.run_steps(&mut test_suite, &mut dictionary, &suite_label)
            .await?;
        self.run_tests(&mut test_suite, &mut dictionary, &suite_label)
            .await?;

        self.run_suite_hooks(
            &test_suite.after_all,
            HookKind::After,
            "After all",
            &mut dictionary,
            &suite_label,
        )
        .await?;

        self.enqueue_msg(SUMMARY.to_string());
        self.finalize_report();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::TestExecutor;
    use vantage_core::result::{RequestType, TestResult};

    fn sample() -> Vec<TestResult> {
        vec![
            TestResult::new(RequestType::Step).with_name("auth".into()),
            TestResult::new(RequestType::Test).with_name("t1".into()),
            TestResult::new(RequestType::Test).with_name("t2".into()),
        ]
    }

    #[test]
    fn stamp_labels_every_result_in_group_mode() {
        let label = Some("getItemQuantityDetails".to_string());
        let stamped = TestExecutor::stamp(sample(), &label);
        assert!(
            stamped
                .iter()
                .all(|r| r.suite.as_deref() == Some("getItemQuantityDetails")),
            "every result (steps included) must carry the suite label"
        );
    }

    #[test]
    fn stamp_is_a_noop_without_a_label() {
        let stamped = TestExecutor::stamp(sample(), &None);
        assert!(stamped.iter().all(|r| r.suite.is_none()));
    }
}
