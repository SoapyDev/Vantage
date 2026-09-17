use clap::Parser;
use reporter::console::ConsoleLogger;
use reporter::html::HtmlReporter;
use runner::step_runner::StepRunner;
use runner::step_runner_compare::StepRunnerCompare;
use runner::step_runner_default::StepRunnerDefault;
use runner::test_runner::TestRunner;
use runner::test_runner_compare::TestRunnerCompare;
use runner::test_runner_default::TestRunnerDefault;
use std::path::PathBuf;
use vantage_core::load::LoadProfile;
use vantage_core::logger::TestLogger;

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None, arg_required_else_help = true)]
#[allow(clippy::struct_excessive_bools)]
pub struct CliArgs {
    #[arg(short, long, conflicts_with = "file")]
    pub group: Option<String>,

    #[arg(short, long, conflicts_with = "group")]
    pub file: Option<String>,

    #[arg(short, long, conflicts_with_all = ["file", "group"])]
    pub sandbox: bool,

    /// Scaffold the workspace and exit: create sandbox/, test-suite/, groups/
    /// and reports/ when missing, and write an annotated sandbox/example.json.
    /// Existing files are left untouched.
    #[arg(long, default_value = "false")]
    pub init: bool,

    /// Environment to run against (validated against the names defined in
    /// `environments.yaml` and embedded at build time, or against the file
    /// named by `VANTAGE_ENVIRONMENTS_FILE` when that override is set).
    #[arg(
        short,
        long,
        default_value = "sandbox",
        value_parser = crate::config::validate_env_name
    )]
    pub environment: String,

    /// Second environment to compare against, enabling compare mode.
    #[arg(
        short,
        long,
        value_parser = crate::config::validate_env_name
    )]
    pub compare_with: Option<String>,

    /// Latency probe with a bounded request budget: warm-up on
    /// expected-fail tests, random pool, sequential then doubling parallel
    /// levels up to --concurrency, adaptive stop on HTTP 429 or >20%
    /// errors. An optional value
    /// overrides the pool size [default: 16]; when larger than the
    /// suite's eligible tests, they are repeated to fill the pool.
    #[arg(
        short,
        long,
        requires = "file",
        conflicts_with_all = ["group", "sandbox", "compare_with"],
        num_args = 0..=1,
        value_name = "POOL_SIZE",
        value_parser = parse_pool_size
    )]
    pub benchmark: Option<Option<usize>>,

    /// Highest parallel level the benchmark escalates to: 1, 2, 4, 8, 16,
    /// 32, 64 or 128. Capped to the machine's cores, rounded down to the
    /// nearest level (e.g. 128 on a 12-core machine runs up to x8).
    #[arg(
        long,
        requires = "benchmark",
        default_value_t = 8,
        value_name = "LEVEL",
        value_parser = parse_concurrency_level
    )]
    pub concurrency: usize,

    /// Open-loop load profile run after the escalation phases (or alone with
    /// `--no-escalation`): a spec of comma-separated stages
    /// (`ramp:3m:30,hold:2m`) or a preset name (smoke, ramp, spike, stairs,
    /// soak). Overrides a `load` block declared in the suite. Requires
    /// `--benchmark`.
    #[arg(
        long,
        requires = "benchmark",
        value_name = "PROFILE|PRESET",
        value_parser = parse_load_profile
    )]
    pub load: Option<LoadProfile>,

    /// Skip the concurrency-escalation phases and run the load profile alone.
    /// Requires a profile (from `--load` or the suite's `load` block).
    #[arg(long, requires = "benchmark", default_value = "false")]
    pub no_escalation: bool,

    /// Cap on concurrent in-flight requests during a load profile, so a slow
    /// server cannot pile up unbounded tasks. Requires `--benchmark`.
    #[arg(
        long,
        requires = "benchmark",
        default_value_t = runner::benchmark::DEFAULT_MAX_IN_FLIGHT,
        value_name = "N",
        value_parser = parse_max_in_flight
    )]
    pub max_in_flight: usize,

    /// Disable the load profile's adaptive stop (HTTP 429, or a per-bucket
    /// error rate above the threshold). Requires `--benchmark`.
    #[arg(long, requires = "benchmark", default_value = "false")]
    pub no_adaptive_stop: bool,

    /// Static pre-flight: resolve every request's templates against the
    /// environment and print what would be sent, without any HTTP. Response
    /// captures are shown as `<captured:name>` placeholders.
    #[arg(long, conflicts_with = "benchmark", default_value = "false")]
    pub dry_run: bool,

    /// Collects per-step timings (load_suite, prepare, http, grade,
    /// extract, hooks, report) via tracing and prints a summary table at
    /// the end of the run. Works in every mode: single file, group,
    /// compare, and benchmark.
    #[arg(long, default_value = "false")]
    pub metrics: bool,

    #[arg(long, default_value = "false")]
    pub verbose: bool,

    /// Parallel suite workers. Default (0): available cores - 1, min 1.
    /// Explicit values are clamped to 1..=available cores.
    #[arg(short, long, default_value_t = 0)]
    pub threads: usize,

    #[arg(short, long, default_value = "false")]
    pub reports: bool,

    /// Comma-separated list of test names to run. Only tests whose `name`
    /// matches an entry are executed; steps still run so captures (e.g. auth)
    /// stay available. A name that matches no test in any suite of the run
    /// prints an error but does not stop the run. Applies to every mode.
    #[arg(short, long, value_delimiter = ',', value_name = "NAME[,NAME...]")]
    pub names: Option<Vec<String>>,

    /// Base directory for generated output (reports go to `<dir>/reports`,
    /// benchmarks to `<dir>/benchmarks`). Overrides the `VANTAGE_OUTPUT_DIR`
    /// environment variable, which overrides the default (the current
    /// directory). Created if missing.
    #[arg(short = 'o', long = "output-dir", value_name = "DIR")]
    pub output_dir: Option<String>,

    /// Email the HTML report(s) to these addresses after the run (comma-
    /// separated). Implies report generation. Requires a build with the
    /// `email` feature and SMTP settings in the environment (SMTP_HOST/PORT/
    /// FROM/USERNAME/PASSWORD, or SMTP_PROVIDER). Subject is the group or
    /// suite name.
    #[cfg(feature = "email")]
    #[arg(long, value_delimiter = ',', value_name = "ADDR[,ADDR...]")]
    pub email: Option<Vec<String>>,
}

impl CliArgs {
    /// Pool size for benchmark mode: `None` when `--benchmark` is absent,
    /// otherwise the explicit value or
    /// [`DEFAULT_POOL_SIZE`](runner::benchmark::DEFAULT_POOL_SIZE).
    #[must_use]
    pub fn benchmark_pool_size(&self) -> Option<usize> {
        self.benchmark
            .map(|explicit| explicit.unwrap_or(runner::benchmark::DEFAULT_POOL_SIZE))
    }

    /// The effective load profile: the `--load` flag wins over a `load` block
    /// declared in the suite (passed as `suite_load`).
    #[must_use]
    pub fn effective_load(&self, suite_load: Option<&LoadProfile>) -> Option<LoadProfile> {
        self.load.clone().or_else(|| suite_load.cloned())
    }

    /// The `--names` selection as a normalized set: entries are trimmed and
    /// blanks dropped. Returns `None` when the flag is absent, and
    /// `Some(empty)` when every entry was blank (e.g. `-n ,,`), which selects
    /// no test rather than silently running all of them.
    #[must_use]
    pub fn requested_names(&self) -> Option<std::collections::HashSet<String>> {
        self.names.as_ref().map(|names| {
            names
                .iter()
                .map(|name| name.trim())
                .filter(|name| !name.is_empty())
                .map(ToString::to_string)
                .collect()
        })
    }

    /// The base directory for generated output, resolved by precedence:
    /// the `--output-dir` flag, then `VANTAGE_OUTPUT_DIR`, then the current
    /// directory. Reports and benchmarks are written to `reports/` and
    /// `benchmarks/` subdirectories of this base.
    #[must_use]
    pub fn output_base(&self) -> PathBuf {
        if let Some(dir) = &self.output_dir {
            return PathBuf::from(dir);
        }
        if let Ok(dir) = std::env::var("VANTAGE_OUTPUT_DIR")
            && !dir.trim().is_empty()
        {
            return PathBuf::from(dir);
        }
        PathBuf::from(".")
    }

    /// Builds the effective arguments for a group *profile*: the profile
    /// string (e.g. `"--benchmark --metrics"`) is parsed as run-mode flags
    /// and merged onto these command-line arguments.
    ///
    /// The command line is layered on top of the profile: for valued options
    /// (`--benchmark`, `--concurrency`, `--threads`) an explicit command-line
    /// value wins, and boolean flags are OR'd (the command line can enable a
    /// mode the profile omitted, but cannot switch one off — profiles have no
    /// negative form). Only run-mode flags are accepted; a profile that tries
    /// to set the environment, a target, or another out-of-scope flag is a
    /// hard error.
    pub fn merged_with_profile(&self, profile: &str) -> anyhow::Result<Self> {
        let tokens: Vec<String> = profile.split_whitespace().map(str::to_string).collect();
        let overrides = ProfileArgs::try_parse_from(&tokens)
            .map_err(|e| anyhow::anyhow!("invalid group profile {profile:?}: {e}"))?;

        let mut effective = self.clone();
        effective.reports = self.reports || overrides.reports;
        effective.metrics = self.metrics || overrides.metrics;
        effective.verbose = self.verbose || overrides.verbose;
        effective.benchmark = self.benchmark.or(overrides.benchmark);
        // `--concurrency` only rides with `--benchmark`; take the command
        // line's value only when the command line owns the benchmark.
        effective.concurrency = if self.benchmark.is_some() {
            self.concurrency
        } else {
            overrides.concurrency
        };
        // Load-profile flags: the command line wins for the valued `--load`,
        // booleans are OR'd, and `--max-in-flight` takes the command line only
        // when it differs from the default (matching the `--concurrency` rule).
        effective.load = self.load.clone().or(overrides.load);
        effective.no_escalation = self.no_escalation || overrides.no_escalation;
        effective.no_adaptive_stop = self.no_adaptive_stop || overrides.no_adaptive_stop;
        effective.max_in_flight = if self.max_in_flight != runner::benchmark::DEFAULT_MAX_IN_FLIGHT
        {
            self.max_in_flight
        } else {
            overrides.max_in_flight
        };
        // 0 means "unset" for threads, so a real command-line value wins.
        effective.threads = if self.threads != 0 {
            self.threads
        } else {
            overrides.threads
        };
        Ok(effective)
    }
}

/// The run-mode flags a group profile may set. Target selectors
/// (`--file`/`--group`/`--sandbox`), the environment, and `--compare-with`
/// are intentionally absent: a profile tunes *how* a group runs, not *where*
/// it points. Parsing a profile string against this struct therefore rejects
/// any out-of-scope flag with a clear error.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
#[allow(clippy::struct_excessive_bools)]
struct ProfileArgs {
    #[arg(short, long)]
    reports: bool,

    #[arg(
        short,
        long,
        num_args = 0..=1,
        value_name = "POOL_SIZE",
        value_parser = parse_pool_size
    )]
    benchmark: Option<Option<usize>>,

    #[arg(
        long,
        requires = "benchmark",
        default_value_t = 8,
        value_name = "LEVEL",
        value_parser = parse_concurrency_level
    )]
    concurrency: usize,

    #[arg(long, requires = "benchmark", value_parser = parse_load_profile)]
    load: Option<LoadProfile>,

    #[arg(long, requires = "benchmark")]
    no_escalation: bool,

    #[arg(
        long,
        requires = "benchmark",
        default_value_t = runner::benchmark::DEFAULT_MAX_IN_FLIGHT,
        value_parser = parse_max_in_flight
    )]
    max_in_flight: usize,

    #[arg(long, requires = "benchmark")]
    no_adaptive_stop: bool,

    #[arg(long)]
    metrics: bool,

    #[arg(long)]
    verbose: bool,

    #[arg(short, long, default_value_t = 0)]
    threads: usize,
}

/// The concurrency levels selectable through `--concurrency` (1 =
/// sequential only; the rest mirror the benchmark escalation).
pub const CONCURRENCY_LEVELS: &[usize] = &[1, 2, 4, 8, 16, 32, 64, 128];

/// clap value parser for `--concurrency`: one of [`CONCURRENCY_LEVELS`].
fn parse_concurrency_level(value: &str) -> Result<usize, String> {
    let level: usize = value.parse().map_err(|e| format!("{e}"))?;
    if CONCURRENCY_LEVELS.contains(&level) {
        Ok(level)
    } else {
        Err(format!(
            "must be one of {}",
            CONCURRENCY_LEVELS
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// clap value parser for the benchmark pool size: a strictly positive
/// integer (a pool of 0 requests could measure nothing).
fn parse_pool_size(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("the pool size must be at least 1".to_string()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

/// clap value parser for `--load`: a spec string of comma-separated stages or
/// a preset name, parsed and validated into a [`LoadProfile`].
fn parse_load_profile(value: &str) -> Result<LoadProfile, String> {
    LoadProfile::parse(value).map_err(|e| e.to_string())
}

/// clap value parser for `--max-in-flight`: a strictly positive integer (a cap
/// of 0 would let nothing run).
fn parse_max_in_flight(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("the in-flight cap must be at least 1".to_string()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

impl From<&CliArgs> for Box<dyn TestLogger + Send> {
    fn from(val: &CliArgs) -> Self {
        if val.reports {
            Box::new(HtmlReporter::new(
                val.output_base().join("reports"),
                val.verbose,
            ))
        } else {
            Box::new(ConsoleLogger::new(val.verbose))
        }
    }
}

impl From<&CliArgs> for Box<dyn StepRunner + Send + Sync> {
    fn from(val: &CliArgs) -> Self {
        if val.compare_with.is_some() {
            Box::new(StepRunnerCompare)
        } else {
            Box::new(StepRunnerDefault)
        }
    }
}

impl From<&CliArgs> for Box<dyn TestRunner + Send + Sync> {
    fn from(val: &CliArgs) -> Self {
        if val.compare_with.is_some() {
            Box::new(TestRunnerCompare)
        } else {
            Box::new(TestRunnerDefault)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CliArgs {
        CliArgs::try_parse_from(args).unwrap()
    }

    #[test]
    fn no_benchmark_flag_means_no_pool_size() {
        let args = parse(&["vantage", "--file", "s.json"]);
        assert_eq!(args.benchmark_pool_size(), None);
    }

    #[test]
    fn benchmark_without_value_uses_the_default_pool_size() {
        let args = parse(&["vantage", "--file", "s.json", "--benchmark"]);
        assert_eq!(
            args.benchmark_pool_size(),
            Some(runner::benchmark::DEFAULT_POOL_SIZE)
        );
    }

    #[test]
    fn benchmark_with_value_overrides_the_pool_size() {
        let args = parse(&["vantage", "--file", "s.json", "--benchmark", "32"]);
        assert_eq!(args.benchmark_pool_size(), Some(32));
    }

    #[test]
    fn benchmark_with_equals_value_overrides_the_pool_size() {
        let args = parse(&["vantage", "--file", "s.json", "--benchmark=64"]);
        assert_eq!(args.benchmark_pool_size(), Some(64));
    }

    #[test]
    fn benchmark_pool_size_of_zero_is_rejected() {
        let outcome = CliArgs::try_parse_from(["vantage", "--file", "s.json", "--benchmark", "0"]);
        assert!(outcome.is_err(), "a zero pool size must be rejected");
    }

    #[test]
    fn concurrency_defaults_to_eight() {
        let args = parse(&["vantage", "--file", "s.json", "--benchmark"]);
        assert_eq!(args.concurrency, 8);
    }

    #[test]
    fn concurrency_accepts_the_documented_levels() {
        for level in ["1", "2", "4", "8", "16", "32", "64", "128"] {
            let args = parse(&[
                "vantage",
                "-f",
                "s.json",
                "--benchmark",
                "--concurrency",
                level,
            ]);
            assert_eq!(args.concurrency.to_string(), level);
        }
    }

    #[test]
    fn concurrency_rejects_values_outside_the_levels() {
        for bad in ["0", "3", "12", "256"] {
            let outcome = CliArgs::try_parse_from([
                "vantage",
                "-f",
                "s.json",
                "--benchmark",
                "--concurrency",
                bad,
            ]);
            assert!(outcome.is_err(), "'{bad}' must be rejected");
        }
    }

    #[test]
    fn concurrency_requires_benchmark_mode() {
        let outcome = CliArgs::try_parse_from(["vantage", "-f", "s.json", "--concurrency", "16"]);
        assert!(
            outcome.is_err(),
            "--concurrency only makes sense with --benchmark"
        );
    }

    #[test]
    fn metrics_flag_is_off_by_default() {
        assert!(!parse(&["vantage", "-f", "s.json"]).metrics);
        assert!(parse(&["vantage", "-f", "s.json", "--metrics"]).metrics);
    }

    #[test]
    fn init_flag_parses_on_its_own() {
        assert!(parse(&["vantage", "--init"]).init);
    }

    #[test]
    fn init_is_off_by_default() {
        assert!(!parse(&["vantage", "-f", "s.json"]).init);
    }

    #[test]
    fn dry_run_flag_parses_and_defaults_off() {
        assert!(parse(&["vantage", "-f", "s.json", "--dry-run"]).dry_run);
        assert!(!parse(&["vantage", "-f", "s.json"]).dry_run);
    }

    #[test]
    fn dry_run_conflicts_with_benchmark() {
        let outcome =
            CliArgs::try_parse_from(["vantage", "-f", "s.json", "--benchmark", "--dry-run"]);
        assert!(
            outcome.is_err(),
            "--dry-run and --benchmark are mutually exclusive"
        );
    }

    #[test]
    fn benchmark_still_requires_a_file() {
        let outcome = CliArgs::try_parse_from(["vantage", "--benchmark"]);
        assert!(outcome.is_err(), "--benchmark requires --file");
    }

    #[test]
    fn names_defaults_to_none() {
        assert!(
            parse(&["vantage", "-f", "s.json"])
                .requested_names()
                .is_none()
        );
    }

    #[test]
    fn names_splits_on_commas() {
        let args = parse(&["vantage", "-f", "s.json", "--names", "TC001,TC002,TC003"]);
        let set = args.requested_names().unwrap();
        assert_eq!(
            set,
            ["TC001", "TC002", "TC003"]
                .into_iter()
                .map(ToString::to_string)
                .collect()
        );
    }

    #[test]
    fn names_short_flag_and_repetition_accumulate() {
        let args = parse(&["vantage", "-f", "s.json", "-n", "a", "-n", "b,c"]);
        let set = args.requested_names().unwrap();
        assert_eq!(
            set,
            ["a", "b", "c"]
                .into_iter()
                .map(ToString::to_string)
                .collect()
        );
    }

    #[test]
    fn names_are_trimmed_and_blanks_dropped() {
        let args = parse(&["vantage", "-f", "s.json", "--names", " TC001 , ,TC002"]);
        let set = args.requested_names().unwrap();
        assert_eq!(
            set,
            ["TC001", "TC002"]
                .into_iter()
                .map(ToString::to_string)
                .collect()
        );
    }

    #[test]
    fn all_blank_names_select_nothing_rather_than_everything() {
        // `Some(empty)` is meaningful: the caller must run zero tests, not all.
        let args = parse(&["vantage", "-f", "s.json", "--names", " , "]);
        assert_eq!(
            args.requested_names(),
            Some(std::collections::HashSet::new())
        );
    }

    #[test]
    fn output_base_precedence_cli_over_env_over_default() {
        // SAFETY: single-threaded manipulation within this test; no other test
        // reads VANTAGE_OUTPUT_DIR.
        unsafe { std::env::remove_var("VANTAGE_OUTPUT_DIR") };
        assert_eq!(
            parse(&["vantage", "-f", "s.json"]).output_base(),
            std::path::PathBuf::from(".")
        );

        unsafe { std::env::set_var("VANTAGE_OUTPUT_DIR", "/tmp/env-out") };
        assert_eq!(
            parse(&["vantage", "-f", "s.json"]).output_base(),
            std::path::PathBuf::from("/tmp/env-out")
        );

        // --output-dir wins over the environment.
        assert_eq!(
            parse(&["vantage", "-f", "s.json", "-o", "/tmp/cli-out"]).output_base(),
            std::path::PathBuf::from("/tmp/cli-out")
        );
        unsafe { std::env::remove_var("VANTAGE_OUTPUT_DIR") };
    }

    #[test]
    fn profile_sets_run_modes_over_the_base() {
        let base = parse(&["vantage", "-g", "g.yaml"]);
        let eff = base.merged_with_profile("--benchmark --metrics").unwrap();
        assert_eq!(
            eff.benchmark_pool_size(),
            Some(runner::benchmark::DEFAULT_POOL_SIZE)
        );
        assert!(eff.metrics);
        assert!(!eff.reports, "a flag the profile omits stays off");
    }

    #[test]
    fn profile_benchmark_pool_and_concurrency_parse() {
        let base = parse(&["vantage", "-g", "g.yaml"]);
        let eff = base
            .merged_with_profile("--benchmark 32 --concurrency 4")
            .unwrap();
        assert_eq!(eff.benchmark_pool_size(), Some(32));
        assert_eq!(eff.concurrency, 4);
    }

    #[test]
    fn command_line_flags_are_layered_over_the_profile() {
        // `--verbose` on the command line augments a profile that only asked
        // for reports.
        let base = parse(&["vantage", "-g", "g.yaml", "--verbose"]);
        let eff = base.merged_with_profile("--reports").unwrap();
        assert!(eff.reports && eff.verbose);
    }

    #[test]
    fn a_profile_with_an_out_of_scope_flag_is_rejected() {
        let base = parse(&["vantage", "-g", "g.yaml"]);
        for bad in ["--environment prod", "--file x.json", "--group other.yaml"] {
            assert!(
                base.merged_with_profile(bad).is_err(),
                "profile '{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn concurrency_in_a_profile_requires_benchmark() {
        let base = parse(&["vantage", "-g", "g.yaml"]);
        assert!(base.merged_with_profile("--concurrency 4").is_err());
    }

    #[test]
    fn an_empty_profile_changes_nothing() {
        let base = parse(&["vantage", "-g", "g.yaml"]);
        let eff = base.merged_with_profile("   ").unwrap();
        assert!(!eff.reports && !eff.metrics && eff.benchmark.is_none());
    }

    // ---------- load-profile flags ----------

    #[test]
    fn load_accepts_a_preset_and_a_spec() {
        let preset = parse(&["vantage", "-f", "s.json", "--benchmark", "--load", "spike"]);
        assert_eq!(preset.load, Some(LoadProfile::parse("spike").unwrap()));

        let spec = parse(&[
            "vantage",
            "-f",
            "s.json",
            "--benchmark",
            "--load",
            "ramp:3m:30,hold:2m",
        ]);
        assert_eq!(
            spec.load,
            Some(LoadProfile::parse("ramp:3m:30,hold:2m").unwrap())
        );
    }

    #[test]
    fn load_requires_benchmark() {
        let outcome = CliArgs::try_parse_from(["vantage", "-f", "s.json", "--load", "smoke"]);
        assert!(outcome.is_err(), "--load only makes sense with --benchmark");
    }

    #[test]
    fn an_invalid_load_spec_is_rejected_at_parse_time() {
        let outcome =
            CliArgs::try_parse_from(["vantage", "-f", "s.json", "--benchmark", "--load", "wat:3m"]);
        assert!(outcome.is_err(), "an unknown shape must be rejected");
    }

    #[test]
    fn no_escalation_and_no_adaptive_stop_require_benchmark() {
        assert!(
            CliArgs::try_parse_from(["vantage", "-f", "s.json", "--no-escalation"]).is_err(),
            "--no-escalation is benchmark-only"
        );
        assert!(
            CliArgs::try_parse_from(["vantage", "-f", "s.json", "--no-adaptive-stop"]).is_err(),
            "--no-adaptive-stop is benchmark-only"
        );
    }

    #[test]
    fn max_in_flight_defaults_parses_and_rejects_zero() {
        let default = parse(&["vantage", "-f", "s.json", "--benchmark"]);
        assert_eq!(
            default.max_in_flight,
            runner::benchmark::DEFAULT_MAX_IN_FLIGHT
        );

        let set = parse(&[
            "vantage",
            "-f",
            "s.json",
            "--benchmark",
            "--max-in-flight",
            "32",
        ]);
        assert_eq!(set.max_in_flight, 32);

        assert!(
            CliArgs::try_parse_from([
                "vantage",
                "-f",
                "s.json",
                "--benchmark",
                "--max-in-flight",
                "0",
            ])
            .is_err(),
            "a zero cap must be rejected"
        );
    }

    #[test]
    fn effective_load_prefers_the_cli_over_the_suite() {
        let suite_profile = LoadProfile::parse("smoke").unwrap();

        let bare = parse(&["vantage", "-f", "s.json", "--benchmark"]);
        assert_eq!(
            bare.effective_load(Some(&suite_profile)),
            Some(suite_profile.clone())
        );

        let overridden = parse(&["vantage", "-f", "s.json", "--benchmark", "--load", "spike"]);
        assert_eq!(
            overridden.effective_load(Some(&suite_profile)),
            Some(LoadProfile::parse("spike").unwrap())
        );

        assert_eq!(bare.effective_load(None), None);
    }

    #[test]
    fn a_group_profile_can_set_the_load_flags() {
        let base = parse(&["vantage", "-g", "g.yaml"]);
        let eff = base
            .merged_with_profile("--benchmark --load spike --no-escalation")
            .unwrap();
        assert_eq!(eff.load, Some(LoadProfile::parse("spike").unwrap()));
        assert!(eff.no_escalation);
    }
}
