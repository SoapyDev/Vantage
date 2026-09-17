use crate::result::TestResult;

pub trait TestLogger {
    /// Gives the logger the suite identity (file path); used by reporters
    /// that produce per-suite artifacts. No-op by default.
    fn set_suite(&mut self, _name: &str) {}

    /// Gives the logger an optional group identity; reporters that aggregate
    /// several suites into one artifact use it as the report title and
    /// directory name. No-op by default.
    fn set_group(&mut self, _name: &str) {}

    /// Gives the logger the run's environment name and, for a compare run, the
    /// compare environment. Reporters use these to label which environment
    /// produced which response. No-op by default.
    fn set_environments(&mut self, _environment: &str, _compare_environment: Option<&str>) {}

    /// Gives the logger the per-step timing metrics (from `--metrics`) as JSON,
    /// so reporters can embed them in their artifact. No-op by default.
    fn set_metrics(&mut self, _metrics: serde_json::Value) {}

    /// The path of the report artifact this logger wrote, if any (e.g. the
    /// HTML report directory). `None` for loggers that produce no file or
    /// before anything is written.
    fn report_path(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn enqueue_msg(&mut self, msg: String);
    fn enqueue(&mut self, result: TestResult, position: usize, total: usize);
    fn log_all(&mut self);
    fn is_verbose(&self) -> bool;
    fn summary(&mut self);
}
