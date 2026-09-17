use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::executor::TestExecutor;
use crate::loader::{load_group, load_test_suite, scan_directory};
use crate::scaffold::{ensure_sandbox_exists, init_workspace_in};
use anyhow::{Context, Error, Result};
use clap::Parser;
use cli_args::CliArgs;
use dotenvy::dotenv;
use reporter::html::HtmlReporter;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use vantage_core::benchmark::BenchReport;
use vantage_core::dictionary::Dictionary;
use vantage_core::logger::TestLogger;
use vantage_core::test_suite::TestSuite;

mod cli_args;
pub mod config;
pub mod constant;
mod dry_run;
mod executor;
mod groups;
mod loader;
mod metrics;
mod scaffold;

/// A logger shared by every suite of a grouped `--reports` run.
type SharedLogger = Arc<Mutex<Box<dyn TestLogger + Send>>>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // A missing .env is fine: secrets may already live in the process
    // environment (e.g. CI). A malformed file would surface when a
    // referenced variable is missing from the resolved dictionary.
    dotenv().ok();

    let args = Arc::new(CliArgs::parse());

    // --init scaffolds the workspace and exits. It runs before any environment
    // or secret resolution, so it works on a fresh checkout with no .env yet.
    if args.init {
        return run_init(&args);
    }

    let execution = with_email_reports(plan_execution(&args)?);

    // The metrics subscriber is global and must be installed before any span
    // fires: do it once if any of the resolved runs asks for metrics.
    let step_timings = execution
        .runs
        .iter()
        .any(|run| run.metrics)
        .then(metrics::install);

    let dictionary = Arc::new(build_dictionary(&args)?);
    let plan = RunPlan {
        files: execution.files,
        group_name: execution.group_name,
    };
    prepare_output_dirs(&args.output_base(), &execution.runs)?;

    let failures = execute_runs(&execution.runs, &plan, &dictionary, step_timings.as_ref()).await?;

    if let Some(timings) = &step_timings {
        println!("{}", timings.render());
    }

    if failures > 0 {
        anyhow::bail!("{failures} suite(s) failed");
    }

    Ok(())
}

/// `--init`: scaffolds the workspace under the resolved output base (so
/// `--output-dir` / `VANTAGE_OUTPUT_DIR` relocates the whole workspace, not
/// just reports) and reports what was created.
fn run_init(args: &CliArgs) -> Result<(), Error> {
    let base = args.output_base();
    let created = init_workspace_in(&base)?;

    if created.is_empty() {
        println!(
            "Workspace already initialized under {}; nothing to create.",
            base.display()
        );
    } else {
        println!("Initialized workspace under {}:", base.display());
        for path in &created {
            println!("  + {path}");
        }
    }

    let example = base.join("sandbox").join("example.json");
    println!(
        "\nEdit {}, then run:  cli -f {}",
        example.display(),
        example.display()
    );
    Ok(())
}

/// `--email` implies report generation so there is an HTML artifact to send.
#[cfg(feature = "email")]
fn with_email_reports(mut execution: Execution) -> Execution {
    for run in &mut execution.runs {
        if run.email.is_some() {
            run.reports = true;
        }
    }
    execution
}

/// Without the `email` feature there is nothing to adjust.
#[cfg(not(feature = "email"))]
fn with_email_reports(execution: Execution) -> Execution {
    execution
}

/// Resolves the shared dictionary once: environment definitions are embedded
/// at build time; secrets come from the process environment (loaded from
/// `.env`). Environment and compare target are command-line level, so every
/// run shares the same dictionary.
fn build_dictionary(args: &CliArgs) -> Result<Dictionary, Error> {
    let environments = config::load()?;
    let env_vars: HashMap<String, String> = std::env::vars().collect();
    Ok(Dictionary::from_config(
        &environments,
        &args.environment,
        args.compare_with.as_deref(),
        &env_vars,
    )?)
}

/// Ensures the output directories exist (and are writable) before running,
/// so a bad `--output-dir` fails fast with a clear message instead of
/// surfacing mid-run when the first report is written.
fn prepare_output_dirs(base: &Path, runs: &[CliArgs]) -> Result<(), Error> {
    if runs.iter().any(|run| !run.dry_run && run.reports) {
        ensure_output_dir(&base.join("reports"))?;
    }
    let benchmarks = runs
        .iter()
        .any(|run| !run.dry_run && run.benchmark_pool_size().is_some());
    if benchmarks {
        ensure_output_dir(&base.join("benchmarks"))?;
    }
    Ok(())
}

/// Runs every resolved profile sequentially: a benchmark wants a quiet
/// machine, and interleaving report output across profiles would be
/// confusing. Returns the number of failed suites.
async fn execute_runs(
    runs: &[CliArgs],
    plan: &RunPlan,
    dictionary: &Arc<Dictionary>,
    step_timings: Option<&Arc<metrics::StepTimings>>,
) -> Result<usize, Error> {
    let mut failures = 0_usize;
    for run in runs {
        // --dry-run is a global offline preview: it wins over a benchmark
        // profile and routes through the (network-free) suite renderer.
        if !run.dry_run
            && let Some(pool_size) = run.benchmark_pool_size()
        {
            run_benchmark_profile(run, plan, dictionary, pool_size, step_timings).await?;
        } else {
            let (delta, report_dir) =
                run_suites(run, plan, dictionary.clone(), step_timings.cloned()).await?;
            failures += delta;
            deliver_report_email(run, report_dir, &suite_run_subject(plan)).await;
        }
    }
    Ok(failures)
}

/// Benchmark characterizes one endpoint at a time; over a group it runs each
/// file in turn, emailing each report as it is written.
async fn run_benchmark_profile(
    run: &CliArgs,
    plan: &RunPlan,
    dictionary: &Dictionary,
    pool_size: usize,
    step_timings: Option<&Arc<metrics::StepTimings>>,
) -> Result<(), Error> {
    for path in &plan.files {
        let report_dir = run_benchmark_file(run, path, dictionary, pool_size, step_timings)
            .await
            .with_context(|| format!("benchmark of '{}' failed", path.display()))?;
        deliver_report_email(run, report_dir, &file_stem_label(path)).await;
    }
    Ok(())
}

/// Runs one profile's suites over `plan`, returning the number of failed
/// suites and, for a grouped `--reports` run, the aggregated report
/// directory. Handles `--dry-run` (static pre-flight), the `--names` filter,
/// and the parallel worker pool.
async fn run_suites(
    args: &CliArgs,
    plan: &RunPlan,
    dictionary: Arc<Dictionary>,
    step_timings: Option<Arc<metrics::StepTimings>>,
) -> Result<(usize, Option<PathBuf>), Error> {
    // Optional `--names` filter, shared across workers.
    let requested_names = Arc::new(args.requested_names());

    if args.dry_run {
        dry_run_suites(plan, dictionary.as_ref(), (*requested_names).as_ref())?;
        return Ok((0, None));
    }

    let group_logger = group_logger_for(args, plan);
    let join_set = spawn_suite_workers(
        &plan.files,
        &Arc::new(args.clone()),
        &dictionary,
        group_logger.as_ref(),
        step_timings.as_ref(),
        &requested_names,
    );
    let (failures, matched_names) = collect_suite_results(join_set).await;

    if let Some(names) = (*requested_names).as_ref() {
        report_unmatched_names(names, &matched_names);
    }

    let report_dir = group_logger
        .as_ref()
        .and_then(|logger| finalize_group_report(logger, step_timings.as_ref()));

    Ok((failures, report_dir))
}

/// `--dry-run`: resolves and prints each suite without sending any request.
/// A suite that fails to load fails the run, mirroring a real run's exit.
fn dry_run_suites(
    plan: &RunPlan,
    dictionary: &Dictionary,
    requested_names: Option<&HashSet<String>>,
) -> Result<(), Error> {
    let mut failures = 0_usize;
    let mut matched = HashSet::new();
    for path in &plan.files {
        match load_test_suite(path) {
            Ok(mut suite) => {
                if let Some(names) = requested_names {
                    matched.extend(suite.retain_tests_named(names));
                    // Nothing selected in this suite: skip it entirely.
                    if suite.tests.is_empty() {
                        continue;
                    }
                }
                println!("{}", dry_run::render(&mut suite, dictionary));
            }
            Err(error) => {
                failures += 1;
                eprintln!("{error:#}");
            }
        }
    }
    if let Some(names) = requested_names {
        report_unmatched_names(names, &matched);
    }
    if failures > 0 {
        anyhow::bail!("{failures} suite(s) failed to load");
    }
    Ok(())
}

/// A grouped run with `--reports` aggregates every suite into a single
/// report, titled and named after the group, via one shared logger.
fn group_logger_for(args: &CliArgs, plan: &RunPlan) -> Option<SharedLogger> {
    match (&plan.group_name, args.reports) {
        (Some(name), true) => {
            let mut reporter = HtmlReporter::new(args.output_base().join("reports"), args.verbose);
            reporter.set_group(name);
            Some(Arc::new(Mutex::new(
                Box::new(reporter) as Box<dyn TestLogger + Send>
            )))
        }
        _ => None,
    }
}

/// Spawns one worker per suite file, bounded by the `--threads` pool.
fn spawn_suite_workers(
    files: &[PathBuf],
    args: &Arc<CliArgs>,
    dictionary: &Arc<Dictionary>,
    group_logger: Option<&SharedLogger>,
    step_timings: Option<&Arc<metrics::StepTimings>>,
    requested_names: &Arc<Option<HashSet<String>>>,
) -> JoinSet<Result<HashSet<String>, Error>> {
    let semaphore = Arc::new(Semaphore::new(get_threads_number(args)));
    let mut join_set = JoinSet::new();

    for path in files {
        let worker = SuiteWorker {
            path: path.clone(),
            args: args.clone(),
            dictionary: dictionary.clone(),
            group_logger: group_logger.cloned(),
            metrics: step_timings.cloned(),
            requested_names: requested_names.clone(),
        };
        let semaphore = semaphore.clone();
        join_set.spawn(async move {
            // The semaphore is never closed, so acquisition cannot fail; map
            // the error anyway rather than panicking.
            let _permit = semaphore
                .acquire()
                .await
                .map_err(|e| anyhow::anyhow!("worker semaphore closed unexpectedly: {e}"))?;
            worker.execute().await
        });
    }
    join_set
}

/// Joins every worker. A single failing suite is reported but does not abort
/// the others; the run as a whole still ends in an error so callers/CI
/// notice. Matched names are unioned across suites so a name found anywhere
/// is not an error.
async fn collect_suite_results(
    mut join_set: JoinSet<Result<HashSet<String>, Error>>,
) -> (usize, HashSet<String>) {
    let mut failures = 0_usize;
    let mut matched_names = HashSet::new();
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(matched)) => matched_names.extend(matched),
            Ok(Err(error)) => {
                failures += 1;
                eprintln!("{error:#}");
            }
            Err(join_error) => {
                failures += 1;
                eprintln!("worker task panicked: {join_error}");
            }
        }
    }
    (failures, matched_names)
}

/// Once every suite has run, embeds the collected metrics and writes the
/// single aggregated group report; returns its directory.
fn finalize_group_report(
    logger: &SharedLogger,
    step_timings: Option<&Arc<metrics::StepTimings>>,
) -> Option<PathBuf> {
    if let Some(timings) = step_timings
        && let Ok(mut locked) = logger.lock()
    {
        locked.set_metrics(timings.as_json());
    }
    if let Ok(mut locked) = logger.lock() {
        tracing::info_span!("report").in_scope(|| locked.log_all());
    }
    logger.lock().ok().and_then(|locked| locked.report_path())
}

/// Everything one suite worker needs, cloned per task.
struct SuiteWorker {
    path: PathBuf,
    args: Arc<CliArgs>,
    dictionary: Arc<Dictionary>,
    group_logger: Option<SharedLogger>,
    metrics: Option<Arc<metrics::StepTimings>>,
    requested_names: Arc<Option<HashSet<String>>>,
}

impl SuiteWorker {
    /// Loads, filters, and executes one suite file; returns the `--names`
    /// entries that matched in it.
    async fn execute(self) -> Result<HashSet<String>, Error> {
        let executor = match self.group_logger {
            Some(logger) => TestExecutor::new_grouped(&self.args, self.dictionary.as_ref(), logger),
            None => TestExecutor::new(&self.args, self.dictionary.as_ref(), self.metrics),
        };
        let mut suite = tracing::info_span!("load_suite").in_scope(|| {
            load_test_suite(&self.path)
                .with_context(|| format!("failed to load suite '{}'", self.path.display()))
        })?;

        // Narrow to the requested test names, remembering what matched here.
        // A suite with no selected test is skipped so its steps do not run
        // for nothing.
        let matched = match (*self.requested_names).as_ref() {
            Some(names) => {
                let matched = suite.retain_tests_named(names);
                if suite.tests.is_empty() {
                    return Ok(matched);
                }
                matched
            }
            None => HashSet::new(),
        };

        executor
            .execute(suite)
            .await
            .with_context(|| format!("suite '{}' failed", self.path.display()))?;
        Ok(matched)
    }
}

/// Requested `--names` that matched no test in any suite of the run, sorted
/// for stable output.
fn unmatched_names(requested: &HashSet<String>, matched: &HashSet<String>) -> Vec<String> {
    let mut missing: Vec<String> = requested.difference(matched).cloned().collect();
    missing.sort();
    missing
}

/// Prints an error line for every requested name that matched nothing. The
/// run has already proceeded with whatever matched, per the `--names`
/// contract (report the miss, keep going).
fn report_unmatched_names(requested: &HashSet<String>, matched: &HashSet<String>) {
    for name in unmatched_names(requested, matched) {
        eprintln!("error: no test named '{name}' matched in any suite");
    }
}

/// Makes sure `dir` exists and is a writable directory, creating it (and any
/// missing parents) if needed. Distinguishes the common failures so the user
/// gets an actionable message.
fn ensure_output_dir(dir: &Path) -> Result<(), Error> {
    if dir.exists() && !dir.is_dir() {
        anyhow::bail!(
            "output path '{}' exists but is not a directory",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create output directory '{}'", dir.display()))?;
    // Probe writability now rather than discovering it when a report is written.
    let probe = dir.join(".vantage-write-test");
    std::fs::write(&probe, [])
        .with_context(|| format!("output directory '{}' is not writable", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn get_threads_number(args: &CliArgs) -> usize {
    let available = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    resolve_thread_count(args.threads, available)
}

/// Resolves the worker count from the `--threads` argument.
///
/// - unspecified (0): all available cores minus one, never below 1;
/// - explicit value: clamped to `1..=available`.
fn resolve_thread_count(requested: usize, available: usize) -> usize {
    if requested == 0 {
        available.saturating_sub(1).max(1)
    } else {
        requested.clamp(1, available)
    }
}

/// Resolves the benchmark concurrency ceiling: the largest selectable level
/// that fits both the `--concurrency` choice and the machine's available
/// cores (e.g. 128 selected on a 12-core machine resolves to 8). Never
/// below 1 (the sequential baseline always runs).
fn resolve_benchmark_concurrency(selected: usize, available: usize) -> usize {
    let ceiling = selected.min(available);
    cli_args::CONCURRENCY_LEVELS
        .iter()
        .rev()
        .find(|&&level| level <= ceiling)
        .copied()
        .unwrap_or(1)
}

/// What to run: the target suite files, the optional group name (used to
/// title an aggregated report), and the one-or-more run profiles to apply.
#[derive(Debug)]
struct Execution {
    files: Vec<PathBuf>,
    group_name: Option<String>,
    runs: Vec<CliArgs>,
}

/// The files to run and the group name, shared by every profile of a run.
#[derive(Debug)]
struct RunPlan {
    files: Vec<PathBuf>,
    group_name: Option<String>,
}

/// Resolves the target files and the run profiles from the arguments.
///
/// `--file` and `--sandbox` yield a single run driven by the command line. A
/// `--group` whose file declares `config` profiles yields one run per profile
/// (the command line merged onto each); a group without config runs once.
fn plan_execution(args: &CliArgs) -> Result<Execution, Error> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut group_name: Option<String> = None;
    // Default: a single run driven entirely by the command line.
    let mut runs: Vec<CliArgs> = vec![args.clone()];

    if args.sandbox {
        let sandbox_path = ensure_sandbox_exists(&args.output_base())?;
        files = scan_directory(&sandbox_path)?;
    } else if let Some(group_path_str) = &args.group {
        let group = load_group(Path::new(group_path_str))?;
        group_name = Some(group.name.clone());
        files = group.files.iter().map(PathBuf::from).collect();

        // Each declared profile becomes a run over the group's files.
        let profiles = group.profiles();
        if !profiles.is_empty() {
            runs = profiles
                .iter()
                .map(|profile| args.merged_with_profile(profile))
                .collect::<Result<Vec<_>, _>>()?;
        }
    } else if let Some(file_path_str) = &args.file {
        files.push(PathBuf::from(file_path_str));
    } else {
        return Err(anyhow::anyhow!(
            "no suite to run: provide --file, --group, or --sandbox"
        ));
    }

    Ok(Execution {
        files,
        group_name,
        runs,
    })
}

/// Benchmarks a single suite file and writes its report under `./benchmarks`.
async fn run_benchmark_file(
    args: &CliArgs,
    path: &Path,
    dictionary: &Dictionary,
    pool_size: usize,
    metrics: Option<&Arc<metrics::StepTimings>>,
) -> Result<Option<PathBuf>, Error> {
    let mut suite = tracing::info_span!("load_suite").in_scope(|| load_test_suite(path))?;
    if !retain_benchmark_tests(&mut suite, args, path) {
        return Ok(None);
    }

    // Resolve the load profile: the CLI `--load` flag wins over a suite-level
    // `load` block. A suite block is only structurally deserialized, so its
    // per-shape rules are checked here.
    let load = args.effective_load(suite.load.as_ref());
    if let Some(profile) = &load {
        profile
            .validate()
            .with_context(|| format!("invalid load profile in '{}'", path.display()))?;
    }
    if args.no_escalation && load.is_none() {
        anyhow::bail!(
            "--no-escalation requires a load profile (via --load or the suite's `load` block)"
        );
    }

    let mut dictionary = dictionary.clone();
    let client = benchmark_client()?;
    let report = runner::benchmark::run_benchmark(
        &client,
        &mut suite,
        &mut dictionary,
        bench_config(args, pool_size, load),
    )
    .await?;

    let label = path.to_string_lossy().into_owned();
    println!("{}", reporter::benchmark::format_report(&label, &report));
    let report_dir = write_benchmark_report(&label, args, &report, metrics)?;
    Ok(Some(report_dir))
}

/// Honors `--names` in benchmark mode too: keeps only the selected tests.
/// Returns `false` when nothing is left to benchmark.
fn retain_benchmark_tests(suite: &mut TestSuite, args: &CliArgs, path: &Path) -> bool {
    let Some(names) = args.requested_names() else {
        return true;
    };
    let matched = suite.retain_tests_named(&names);
    report_unmatched_names(&names, &matched);
    if suite.tests.is_empty() {
        println!("No matching tests to benchmark in '{}'.", path.display());
        return false;
    }
    true
}

/// The benchmark HTTP client, with a per-request timeout: a hung endpoint
/// must fail its sample, not freeze the whole benchmark.
fn benchmark_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build the benchmark HTTP client")
}

/// Resolves the benchmark tuning: the machine-capped concurrency ceiling, a
/// time-derived shuffle seed, and the load-profile flags. `load` is the
/// already-resolved effective profile (CLI over suite).
fn bench_config(
    args: &CliArgs,
    pool_size: usize,
    load: Option<vantage_core::load::LoadProfile>,
) -> runner::benchmark::BenchConfig {
    let available = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    runner::benchmark::BenchConfig {
        pool_size,
        max_concurrency: Some(resolve_benchmark_concurrency(args.concurrency, available)),
        seed,
        load,
        escalation: !args.no_escalation,
        max_in_flight: args.max_in_flight,
        adaptive_stop: !args.no_adaptive_stop,
    }
}

/// Writes the benchmark report files and prints where they landed.
fn write_benchmark_report(
    label: &str,
    args: &CliArgs,
    report: &BenchReport,
    metrics: Option<&Arc<metrics::StepTimings>>,
) -> Result<PathBuf, Error> {
    let report_dir = tracing::info_span!("report").in_scope(|| {
        let metrics_json = metrics.map_or(serde_json::Value::Null, |t| t.as_json());
        reporter::benchmark::write_report_files(
            label,
            &args.environment,
            report,
            metrics_json,
            &args.output_base().join("benchmarks"),
        )
    })?;
    println!("Rapport : {}", report_dir.join("index.html").display());
    Ok(report_dir)
}

/// Subject for a suite/group run: the group name, else the single file's stem.
fn suite_run_subject(plan: &RunPlan) -> String {
    if let Some(group) = &plan.group_name {
        return group.clone();
    }
    plan.files
        .first()
        .map(|p| file_stem_label(p))
        .unwrap_or_else(|| "vantage".to_string())
}

/// A filesystem-stem label for a suite file, used as an email subject.
fn file_stem_label(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("suite")
        .to_string()
}

/// Emails a written report to the `--email` recipients. Compiles to a no-op
/// (that swallows its arguments) without the `email` feature.
#[cfg(not(feature = "email"))]
#[allow(clippy::unused_async)]
async fn deliver_report_email(_args: &CliArgs, _report_dir: Option<PathBuf>, _subject: &str) {}

#[cfg(feature = "email")]
async fn deliver_report_email(args: &CliArgs, report_dir: Option<PathBuf>, subject: &str) {
    let Some(recipients) = args.email.clone().filter(|r| !r.is_empty()) else {
        return;
    };
    let Some(dir) = report_dir else {
        return;
    };
    let Some(config) = mail_config() else {
        return;
    };

    let report = mailer::OutgoingReport {
        recipients: recipients.clone(),
        subject: subject.to_string(),
        body: report_summary(&dir).unwrap_or_else(|| format!("vantage report: {subject}")),
        attachment_path: dir.join("index.html"),
        attachment_name: "index.html".to_string(),
    };

    match mailer::send(config, report).await {
        Ok(()) => println!("Report emailed to {}", recipients.join(", ")),
        Err(e) => eprintln!("warning: emailing the report failed: {e:#}"),
    }
}

/// The mail configuration, read from the environment once on first use and
/// cached. Returns `None` (warning once) if the environment is incomplete, so
/// a misconfigured mailer disables email rather than failing the run.
#[cfg(feature = "email")]
fn mail_config() -> Option<&'static mailer::MailConfig> {
    use std::sync::OnceLock;
    static CONFIG: OnceLock<Option<mailer::MailConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| match mailer::MailConfig::from_env() {
            Ok(config) => Some(config),
            Err(e) => {
                eprintln!("warning: email disabled ({e:#})");
                None
            }
        })
        .as_ref()
}

/// A one-line body from the report's `data.json` meta (pass/fail + env), or
/// `None` for reports without those fields (e.g. benchmarks).
#[cfg(feature = "email")]
fn report_summary(dir: &Path) -> Option<String> {
    let data = std::fs::read_to_string(dir.join("data.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    let meta = value.get("meta")?;
    let passed = meta.get("passed")?.as_u64()?;
    let total = meta.get("total")?.as_u64()?;
    let environment = meta
        .get("environment")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    Some(format!(
        "{passed}/{total} tests passed on {environment}. Full report attached."
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        Execution, ensure_output_dir, plan_execution, resolve_benchmark_concurrency,
        resolve_thread_count, unmatched_names,
    };
    use crate::cli_args::CliArgs;
    use clap::Parser;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn unmatched_names_reports_only_names_matched_nowhere_sorted() {
        let requested = set(&["c", "a", "b"]);
        let matched = set(&["b"]);
        assert_eq!(unmatched_names(&requested, &matched), vec!["a", "c"]);
    }

    #[test]
    fn unmatched_names_is_empty_when_everything_matched() {
        let requested = set(&["a", "b"]);
        let matched = set(&["a", "b", "extra"]);
        assert!(unmatched_names(&requested, &matched).is_empty());
    }

    #[test]
    fn plan_execution_without_a_target_is_an_error() {
        // Valid parse (--metrics satisfies arg_required_else_help) but no
        // --file/--group/--sandbox to run.
        let args = CliArgs::try_parse_from(["vantage", "--metrics"]).unwrap();
        let err = plan_execution(&args).unwrap_err();
        assert!(err.to_string().contains("--file"), "{err}");
    }

    #[test]
    fn plan_execution_with_a_single_file_returns_one_run() {
        let args = CliArgs::try_parse_from(["vantage", "--file", "s.json"]).unwrap();
        let Execution {
            files,
            group_name,
            runs,
        } = plan_execution(&args).unwrap();
        assert_eq!(files, vec![PathBuf::from("s.json")]);
        assert!(group_name.is_none());
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn plan_execution_expands_group_config_into_one_run_per_profile() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("vantage_group_cfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let group_path = dir.join("g.yaml");
        let mut file = std::fs::File::create(&group_path).unwrap();
        writeln!(
            file,
            "name: G\nconfig:\n  - \"--reports\"\n  - \"--benchmark --metrics\"\nfiles:\n  - ./a.json\n  - ./b.json"
        )
        .unwrap();

        let args =
            CliArgs::try_parse_from(["vantage", "--group", group_path.to_str().unwrap()]).unwrap();
        let exec = plan_execution(&args).unwrap();

        assert_eq!(exec.files.len(), 2);
        assert_eq!(exec.group_name.as_deref(), Some("G"));
        assert_eq!(exec.runs.len(), 2, "one run per config profile");
        assert!(exec.runs[0].reports);
        assert_eq!(
            exec.runs[1].benchmark_pool_size(),
            Some(runner::benchmark::DEFAULT_POOL_SIZE)
        );
        assert!(exec.runs[1].metrics);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_output_dir_creates_missing_dirs() {
        let base = std::env::temp_dir().join(format!("vantage_out_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("reports").join("nested");
        ensure_output_dir(&dir).unwrap();
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_output_dir_rejects_a_file_path() {
        let base = std::env::temp_dir().join(format!("vantage_outf_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(ensure_output_dir(&file).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn benchmark_concurrency_rounds_down_to_a_level_within_the_cores() {
        // 128 requested but 12 cores -> largest level <= 12 is 8.
        assert_eq!(resolve_benchmark_concurrency(128, 12), 8);
    }

    #[test]
    fn benchmark_concurrency_keeps_the_selected_level_when_cores_allow() {
        assert_eq!(resolve_benchmark_concurrency(16, 16), 16);
        assert_eq!(resolve_benchmark_concurrency(32, 64), 32);
    }

    #[test]
    fn benchmark_concurrency_never_goes_below_sequential() {
        assert_eq!(resolve_benchmark_concurrency(8, 1), 1);
        assert_eq!(resolve_benchmark_concurrency(1, 64), 1);
    }

    #[test]
    fn default_uses_available_minus_one() {
        assert_eq!(resolve_thread_count(0, 8), 7);
    }

    #[test]
    fn default_never_goes_below_one() {
        assert_eq!(resolve_thread_count(0, 1), 1);
        assert_eq!(resolve_thread_count(0, 2), 1);
    }

    #[test]
    fn explicit_value_within_range_is_kept() {
        assert_eq!(resolve_thread_count(5, 8), 5);
        assert_eq!(resolve_thread_count(1, 8), 1);
    }

    #[test]
    fn explicit_value_is_clamped_to_available() {
        assert_eq!(resolve_thread_count(64, 8), 8);
        assert_eq!(resolve_thread_count(2, 1), 1);
    }
}
