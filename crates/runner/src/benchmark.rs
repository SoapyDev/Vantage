//! `--benchmark` mode: characterizes an endpoint's latency profile with a
//! fixed, bounded request budget (the target servers are rate limited).
//!
//! Protocol:
//! 1. Suite steps run once (auth) - excluded from stats.
//! 2. Latency probe: [`LATENCY_PROBES`] raw TCP connects (network
//!    round-trip floor) and as many minimal HTTP requests (stack floor,
//!    including TLS/proxy/framework) against the suite URL. Reported as
//!    context, never subtracted from the measured stats.
//! 3. Warm-up: tests *expected to fail* (`expected_status >= 400`) are fired
//!    once, sequentially, then discarded - they only wake the service up.
//! 4. Pool: a seeded shuffle of the expected-success tests, truncated to the
//!    requested pool size ([`DEFAULT_POOL_SIZE`] unless overridden). When the
//!    requested size exceeds the eligible tests, the shuffled set is cycled
//!    so each test runs multiple times per phase. `for_each` tests are
//!    excluded.
//! 5. Phases: the *same pool* runs sequentially, then at doubling
//!    concurrency levels ([`PARALLEL_LEVELS`], up to x128). The escalation
//!    stops before any level above `max_concurrency` (from `--concurrency`,
//!    machine-capped by the CLI) or above the pool size (which could not
//!    add parallelism).
//! 6. Adaptive stop: any HTTP 429, or an error rate above
//!    [`ERROR_RATE_STOP`], halts the escalation - no requests are wasted
//!    confirming a ceiling that has already been found.
//!
//! Worst-case budget: steps + warm-up + (phases run) x pool; with the
//! default pool of 16 and the default x8 ceiling, ~70-80 requests.
//! Bodies and headers are dropped on arrival; only status + duration are
//! kept.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use vantage_core::benchmark::{
    BenchReport, BenchSample, LatencyReport, LoadReport, PhaseReport, ServerSpan,
};
use vantage_core::dictionary::Dictionary;
use vantage_core::load::LoadProfile;
use vantage_core::request::TestRequest;
use vantage_core::result::RequestType;
use vantage_core::template_engine::injector::Injector;
use vantage_core::test_suite::{SuiteConfig, TestSuite};

use crate::step_runner::StepRunner;
use crate::step_runner_default::StepRunnerDefault;
use crate::{handle_request, make_request};

/// Default size of the benchmark pool, used when `--benchmark` is passed
/// without a value. Pass `--benchmark <N>` for bigger samples (e.g. 50 for
/// a usable P90) when targeting services without rate limiting.
pub const DEFAULT_POOL_SIZE: usize = 16;

/// Concurrency levels tried after the sequential baseline.
pub const PARALLEL_LEVELS: &[usize] = &[2, 4, 8, 16, 32, 64, 128];

/// Error-rate threshold above which the escalation stops.
pub const ERROR_RATE_STOP: f64 = 0.20;

/// Default cap on concurrent in-flight requests during a load profile: keeps a
/// slow server from letting the scheduler pile up unbounded tasks.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 256;

/// Scheduler tick: the budget of due arrivals is recomputed this often. Coarse
/// enough to avoid per-request sleeps and OS timer-resolution issues, fine
/// enough to smooth the arrival curve.
const LOAD_TICK: Duration = Duration::from_millis(20);

/// Minimum completions in a one-second bucket before its error rate is allowed
/// to trip the adaptive stop, so a single early failure cannot halt the run.
const LOAD_ADAPTIVE_MIN_SAMPLES: usize = 8;

/// Number of samples taken by each latency probe (TCP and HTTP).
pub const LATENCY_PROBES: usize = 10;

/// Per-probe timeout. A host that accepts connections but never answers
/// must not hang the benchmark (10 probes at OS connect timeouts could
/// otherwise take minutes).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Extracts the `(host, port)` to probe from the resolved suite URL, using
/// the scheme's default port when none is explicit.
fn probe_target(url: &str) -> Option<(String, u16)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port_or_known_default()?;
    Some((host, port))
}

/// Times raw TCP connects to `host:port`. A first throwaway connect warms
/// the DNS cache so resolution does not pollute the first sample. Gives up
/// at the first failed or timed-out connect: further probes of a dead host
/// would only stack more timeouts.
async fn measure_tcp(host: &str, port: u16, timeout: Duration) -> Vec<u128> {
    let addr = format!("{host}:{port}");
    let _ = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await;

    let mut samples = Vec::with_capacity(LATENCY_PROBES);
    for _ in 0..LATENCY_PROBES {
        let start = Instant::now();
        match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await {
            Ok(Ok(stream)) => {
                samples.push(start.elapsed().as_micros());
                drop(stream);
            }
            _ => break,
        }
    }
    samples
}

/// Times minimal HTTP round-trips (HEAD) against the suite URL. The status
/// code does not matter (a 405 still measures a full round trip); a first
/// throwaway request warms the connection (DNS, TCP, TLS). Gives up at the
/// first failed or timed-out request.
async fn measure_http(client: &Client, url: &str, timeout: Duration) -> Vec<u128> {
    let _ = tokio::time::timeout(timeout, client.head(url).send()).await;

    let mut samples = Vec::with_capacity(LATENCY_PROBES);
    for _ in 0..LATENCY_PROBES {
        let start = Instant::now();
        match tokio::time::timeout(timeout, client.head(url).send()).await {
            Ok(Ok(_)) => samples.push(start.elapsed().as_micros()),
            _ => break,
        }
    }
    samples
}

/// Runs both latency probes against the resolved suite URL. Returns `None`
/// when the URL cannot be parsed into a probe target.
async fn measure_latency(client: &Client, url: &str) -> Option<LatencyReport> {
    let (host, port) = probe_target(url)?;
    Some(LatencyReport {
        tcp_us: measure_tcp(&host, port, PROBE_TIMEOUT).await,
        http_us: measure_http(client, url, PROBE_TIMEOUT).await,
    })
}

/// The processing time extracted from a `Server-Timing` header.
struct ServerTiming {
    /// Total processing: the metric named `total` when present, otherwise
    /// the largest `dur` (nested spans must not be double-counted).
    server_ms: f64,
    /// The other named metrics, only when an explicit `total` exists -
    /// without it we cannot know whether spans nest or follow each other.
    parts: Vec<ServerSpan>,
}

/// Parses one `Server-Timing` metric (e.g. `app;dur=10.2`); `None` without a
/// parseable `dur`.
fn parse_timing_metric(metric: &str) -> Option<ServerSpan> {
    let mut sections = metric.split(';');
    let name = sections.next()?.trim().to_string();
    let dur_ms = sections.find_map(|part| {
        let part = part.trim();
        part.get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("dur="))
            .and_then(|_| part[4..].parse::<f64>().ok())
    })?;
    Some(ServerSpan { name, dur_ms })
}

/// Parses a `Server-Timing` header (e.g. `total;dur=27.2,app;dur=10.2`).
/// Returns `None` when the header is absent or carries no parseable `dur`.
fn parse_server_timing(headers: &HashMap<String, String>) -> Option<ServerTiming> {
    let raw = headers.get("server-timing")?;
    let mut metrics: Vec<ServerSpan> = raw.split(',').filter_map(parse_timing_metric).collect();

    if let Some(position) = metrics
        .iter()
        .position(|m| m.name.eq_ignore_ascii_case("total"))
    {
        let total = metrics.remove(position);
        return Some(ServerTiming {
            server_ms: total.dur_ms,
            parts: metrics,
        });
    }

    let largest = metrics
        .iter()
        .map(|m| m.dur_ms)
        .fold(None, |largest: Option<f64>, dur| {
            Some(largest.map_or(dur, |l| l.max(dur)))
        })?;
    Some(ServerTiming {
        server_ms: largest,
        parts: vec![],
    })
}

/// Splits the suite's tests into (warm-up, pool).
///
/// Warm-up = tests expected to fail (`expected_status >= 400`).
/// Pool = seeded shuffle of the expected-success tests, truncated to
/// `pool_size`; when `pool_size` exceeds them, the shuffled set is cycled so
/// each test runs multiple times. Tests carrying a `for_each` are excluded
/// from both.
#[must_use]
pub fn select_pool(
    tests: &[TestRequest],
    pool_size: usize,
    seed: u64,
) -> (Vec<TestRequest>, Vec<TestRequest>) {
    // Partition by reference so the eligible tests are never deep-cloned here;
    // `build_pool` clones each chosen one exactly once, into its slot. Only the
    // (small) warm-up set is materialized.
    let (warmup, eligible): (Vec<&TestRequest>, Vec<&TestRequest>) = tests
        .iter()
        .filter(|t| t.for_each.is_none())
        .partition(|t| t.expected_status >= 400);

    let pool = build_pool(&eligible, pool_size, seed);
    (warmup.into_iter().cloned().collect(), pool)
}

/// Builds the pool by shuffling *indices* into `items` (a Fisher-Yates driven
/// by a xorshift64 PRNG, no extra dependency), then cloning each chosen test
/// exactly once, directly into its slot. `max` slots are filled: when `max`
/// exceeds the number of eligible items the shuffled order is cycled, so every
/// item is repeated a balanced number of times (n or n+1).
///
/// The swap sequence is identical to shuffling the tests themselves, so the
/// pool is byte-for-byte what the previous owned shuffle produced -- only the
/// clone count drops, from one per eligible test plus one per slot down to one
/// per slot.
fn build_pool(items: &[&TestRequest], max: usize, seed: u64) -> Vec<TestRequest> {
    let n = items.len();
    // Full shuffle when cycling, partial (first `max`) otherwise. `take == 0`
    // (no eligible tests, or an empty pool) short-circuits the modulo below.
    let take = max.min(n);
    if take == 0 {
        return Vec::new();
    }

    let mut order: Vec<usize> = (0..n).collect();
    let mut state = seed.max(1);
    for i in 0..take {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = i + (state as usize) % (n - i);
        order.swap(i, j);
    }

    (0..max)
        .map(|slot| items[order[slot % take]].clone())
        .collect()
}

async fn execute_one(
    client: &Client,
    mut request: TestRequest,
    variables: &HashMap<String, Value>,
) -> Result<BenchSample, anyhow::Error> {
    use tracing::Instrument;

    let builder = tracing::info_span!("prepare")
        .in_scope(|| make_request(client, &mut request, variables))?;
    let result = handle_request(&request, builder, RequestType::Test)
        .instrument(tracing::info_span!("http"))
        .await;

    let timing = parse_server_timing(&result.headers);
    let (server_ms, server_parts) = match timing {
        Some(t) => (Some(t.server_ms), t.parts),
        None => (None, vec![]),
    };

    Ok(BenchSample {
        name: request.name,
        offset_ms: 0,
        duration_ms: result.duration,
        status: result.status,
        is_success: result.status == Some(request.expected_status),
        server_ms,
        server_parts,
    })
}

/// Runs the pool once at the given concurrency and times the whole phase.
async fn run_phase(
    client: &Client,
    pool: &[TestRequest],
    variables: &Arc<HashMap<String, Value>>,
    concurrency: usize,
) -> Result<PhaseReport, anyhow::Error> {
    let start = Instant::now();

    let samples = if concurrency <= 1 {
        run_pool_sequential(client, pool, variables).await?
    } else {
        run_pool_concurrent(client, pool, variables, concurrency).await?
    };

    Ok(PhaseReport {
        concurrency,
        wall_ms: start.elapsed().as_millis(),
        samples,
    })
}

/// The sequential baseline: one request at a time, in pool order.
async fn run_pool_sequential(
    client: &Client,
    pool: &[TestRequest],
    variables: &Arc<HashMap<String, Value>>,
) -> Result<Vec<BenchSample>, anyhow::Error> {
    let mut samples = Vec::with_capacity(pool.len());
    for template in pool {
        samples.push(execute_one(client, template.clone(), variables).await?);
    }
    Ok(samples)
}

/// Runs the pool with at most `concurrency` requests in flight.
async fn run_pool_concurrent(
    client: &Client,
    pool: &[TestRequest],
    variables: &Arc<HashMap<String, Value>>,
    concurrency: usize,
) -> Result<Vec<BenchSample>, anyhow::Error> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut join_set = JoinSet::new();

    for template in pool {
        let client = client.clone();
        let template = template.clone();
        let variables = variables.clone();
        let semaphore = semaphore.clone();

        join_set.spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();
            execute_one(&client, template, &variables).await
        });
    }

    let mut samples = Vec::with_capacity(pool.len());
    while let Some(sample) = join_set.join_next().await {
        samples.push(sample.expect("benchmark task panicked")?);
    }
    Ok(samples)
}

fn check_phase(phase: &PhaseReport) -> Option<String> {
    if phase.rate_limited() {
        return Some(format!(
            "HTTP 429 received at concurrency {} - the server is rate limiting",
            phase.concurrency
        ));
    }

    if phase.error_rate() > ERROR_RATE_STOP {
        return Some(format!(
            "{:.0}% errors at concurrency {} (threshold: {:.0}%)",
            phase.error_rate() * 100.0,
            phase.concurrency,
            ERROR_RATE_STOP * 100.0
        ));
    }

    None
}

/// Tuning of a [`run_benchmark`] run.
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// Number of pooled requests per phase (the CLI defaults to
    /// [`DEFAULT_POOL_SIZE`]).
    pub pool_size: usize,
    /// Highest concurrency level to escalate to (from `--concurrency`,
    /// machine-capped by the CLI); `None` = every level. Levels above the
    /// pool size are skipped either way, since they could not put more
    /// requests in flight than the pool holds.
    pub max_concurrency: Option<usize>,
    /// Drives the pool shuffle; pass a fixed value for determinism.
    pub seed: u64,
    /// When set, an open-loop load profile is run (after the escalation
    /// phases, or instead of them when `escalation` is off).
    pub load: Option<LoadProfile>,
    /// Run the concurrency-escalation phases. Off (via `--no-escalation`) runs
    /// the load profile alone.
    pub escalation: bool,
    /// Cap on concurrent in-flight requests during the load profile
    /// (the CLI defaults to [`DEFAULT_MAX_IN_FLIGHT`]).
    pub max_in_flight: usize,
    /// Apply the adaptive stop (HTTP 429 or a per-bucket error rate above
    /// [`ERROR_RATE_STOP`]) to the load profile.
    pub adaptive_stop: bool,
}

/// Runs the full benchmark protocol against a suite.
pub async fn run_benchmark(
    client: &Client,
    suite: &mut TestSuite,
    dictionary: &mut Dictionary,
    config: BenchConfig,
) -> Result<BenchReport, anyhow::Error> {
    if !suite.steps.is_empty() {
        StepRunnerDefault.run(client, suite, dictionary).await?;
    }

    let suite_config: SuiteConfig = (&mut *suite).into();
    let (mut warmup, mut pool) = select_pool(&suite.tests, config.pool_size, config.seed);
    for test in warmup.iter_mut().chain(pool.iter_mut()) {
        test.update_from(&suite_config);
    }

    if pool.is_empty() {
        anyhow::bail!("benchmark: no eligible test in the suite (expected-success, non-for_each)");
    }

    let variables = Arc::new(dictionary.variables.clone());

    let url = Injector::inject_str(&suite.url, &dictionary.variables)
        .unwrap_or_else(|| suite.url.clone());
    let latency = measure_latency(client, &url).await;

    // Warm-up: timing results intentionally discarded, but a build error
    // (e.g. unsupported content type) still aborts the benchmark.
    let warmup_count = warmup.len();
    for template in warmup {
        execute_one(client, template, &variables).await?;
    }

    let (phases, stop_reason) = if config.escalation {
        run_phases(client, &pool, &variables, config.max_concurrency).await?
    } else {
        (Vec::new(), None)
    };

    let load = if let Some(profile) = &config.load {
        let (samples, load_stop) = run_load_profile(
            client,
            &pool,
            &variables,
            profile,
            config.max_in_flight,
            config.adaptive_stop,
        )
        .await?;
        Some(LoadReport::new(profile, samples, load_stop))
    } else {
        None
    };

    Ok(BenchReport {
        warmup_count,
        pool_size: pool.len(),
        latency,
        phases,
        stop_reason,
        load,
    })
}

/// Runs the open-loop load profile: it schedules requests so the number sent
/// tracks the curve's cumulative arrivals `N(t)`, drawing from the pool
/// round-robin, capped at `max_in_flight` concurrent calls. Returns every
/// sample (stamped with its dispatch offset) and the adaptive-stop reason, if
/// any.
///
/// Scheduling is budget-based rather than per-request sleeps: on each
/// [`LOAD_TICK`] it fires until the sent count catches up to `N(elapsed)`. When
/// the in-flight cap is reached the outstanding arrivals are dropped (advancing
/// the budget) instead of bursting later — the coordinated omission that
/// results shows up as a gap between the target and achieved lines rather than
/// being hidden.
async fn run_load_profile(
    client: &Client,
    pool: &[TestRequest],
    variables: &Arc<HashMap<String, Value>>,
    profile: &LoadProfile,
    max_in_flight: usize,
    adaptive_stop: bool,
) -> Result<(Vec<BenchSample>, Option<String>), anyhow::Error> {
    let curve = profile.compile();
    let total = curve.total_duration();
    let semaphore = Arc::new(Semaphore::new(max_in_flight.max(1)));
    let samples: Arc<Mutex<Vec<BenchSample>>> = Arc::default();
    let first_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::default();
    let mut join_set = JoinSet::new();

    let start = Instant::now();
    // `sent` counts arrivals accounted for (fired or dropped by the cap), so
    // the budget stays open-loop and never backlogs.
    let mut sent: u64 = 0;
    let mut stop_reason = None;
    let mut checked_secs = 0;

    loop {
        let elapsed = start.elapsed();
        let due = curve.cumulative_arrivals(elapsed) as u64;

        while sent < due {
            match Arc::clone(&semaphore).try_acquire_owned() {
                Ok(permit) => {
                    let offset_ms = elapsed.as_millis();
                    let template = pool[(sent as usize) % pool.len()].clone();
                    let client = client.clone();
                    let variables = variables.clone();
                    let samples = samples.clone();
                    let first_error = first_error.clone();
                    join_set.spawn(async move {
                        let _permit = permit;
                        match execute_one(&client, template, &variables).await {
                            Ok(mut sample) => {
                                sample.offset_ms = offset_ms;
                                samples.lock().expect("load samples lock").push(sample);
                            }
                            Err(error) => {
                                let mut slot = first_error.lock().expect("load error lock");
                                if slot.is_none() {
                                    *slot = Some(error);
                                }
                            }
                        }
                    });
                    sent += 1;
                }
                Err(_) => {
                    // In-flight cap reached: drop the arrivals due now and
                    // advance the budget so no catch-up burst follows.
                    sent = due;
                    break;
                }
            }
        }

        // A build error (e.g. an unsupported content type) is deterministic;
        // abort rather than keep firing doomed requests.
        if first_error.lock().expect("load error lock").is_some() {
            break;
        }

        // Adaptive stop, evaluated at most once per elapsed second.
        let secs = elapsed.as_secs();
        if adaptive_stop && secs > checked_secs {
            checked_secs = secs;
            stop_reason = load_stop_reason(&samples.lock().expect("load samples lock"));
        }
        if stop_reason.is_some() || elapsed >= total {
            break;
        }

        tokio::time::sleep(LOAD_TICK).await;
    }

    // Stop scheduling; let the in-flight requests finish.
    while join_set.join_next().await.is_some() {}

    if let Some(error) = first_error.lock().expect("load error lock").take() {
        return Err(error);
    }

    let mut samples = std::mem::take(&mut *samples.lock().expect("load samples lock"));
    samples.sort_by_key(|s| s.offset_ms);

    // Re-check over the whole run: the deciding bucket may have completed only
    // during the drain.
    if adaptive_stop && stop_reason.is_none() {
        stop_reason = load_stop_reason(&samples);
    }

    Ok((samples, stop_reason))
}

/// The adaptive-stop reason for the samples gathered so far, or `None`.
///
/// Any HTTP 429 halts immediately (the server is rate limiting). Otherwise the
/// completions are bucketed by their completion second and the first bucket
/// with at least [`LOAD_ADAPTIVE_MIN_SAMPLES`] completions and an error rate
/// above [`ERROR_RATE_STOP`] halts the run.
fn load_stop_reason(samples: &[BenchSample]) -> Option<String> {
    if samples.iter().any(|s| s.status == Some(429)) {
        return Some("HTTP 429 during the load profile - the server is rate limiting".to_string());
    }

    let mut buckets: HashMap<u64, (usize, usize)> = HashMap::new();
    for sample in samples {
        let second = ((sample.offset_ms + sample.duration_ms) / 1000) as u64;
        let entry = buckets.entry(second).or_default();
        entry.0 += 1;
        if !sample.is_success {
            entry.1 += 1;
        }
    }

    buckets
        .into_iter()
        .filter(|(_, (total, _))| *total >= LOAD_ADAPTIVE_MIN_SAMPLES)
        .find_map(|(second, (total, errors))| {
            let rate = errors as f64 / total as f64;
            (rate > ERROR_RATE_STOP).then(|| {
                format!(
                    "{:.0}% errors in second {second} of the load profile (threshold: {:.0}%)",
                    rate * 100.0,
                    ERROR_RATE_STOP * 100.0
                )
            })
        })
}

/// Runs the sequential baseline, then escalates through the doubling
/// parallel levels until a cap or an adaptive stop is hit. Returns the
/// measured phases and the reason the escalation stopped, if any.
async fn run_phases(
    client: &Client,
    pool: &[TestRequest],
    variables: &Arc<HashMap<String, Value>>,
    max_concurrency: Option<usize>,
) -> Result<(Vec<PhaseReport>, Option<String>), anyhow::Error> {
    let mut phases: Vec<PhaseReport> = vec![];

    // Sequential baseline.
    let phase = run_phase(client, pool, variables, 1).await?;
    let mut stop_reason = check_phase(&phase);
    phases.push(phase);

    if stop_reason.is_none() {
        for &level in PARALLEL_LEVELS {
            if let Some(reason) = level_cap_reason(level, max_concurrency, pool.len()) {
                stop_reason = Some(reason);
                break;
            }

            let phase = run_phase(client, pool, variables, level).await?;
            stop_reason = check_phase(&phase);
            phases.push(phase);

            if stop_reason.is_some() {
                break;
            }
        }
    }

    Ok((phases, stop_reason))
}

/// Why the escalation must stop *before* running `level`, if it must: the
/// `--concurrency` cap, or a level that exceeds the pool size (which could
/// not put more requests in flight than the pool holds).
fn level_cap_reason(
    level: usize,
    max_concurrency: Option<usize>,
    pool_len: usize,
) -> Option<String> {
    if let Some(max) = max_concurrency
        && level > max
    {
        return Some(format!("concurrency capped at {max}"));
    }
    if level > pool_len {
        return Some(format!(
            "concurrency {level} exceeds the pool size ({pool_len}); raise --benchmark to go higher"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_named(name: &str, expected_status: u16) -> TestRequest {
        serde_json::from_value(json!({
            "name": name,
            "payload": {},
            "expected_status": expected_status
        }))
        .unwrap()
    }

    fn looped(name: &str) -> TestRequest {
        serde_json::from_value(json!({
            "name": name,
            "payload": {},
            "for_each": {"in": "items"}
        }))
        .unwrap()
    }

    #[test]
    fn warmup_collects_expected_failures_and_pool_expected_successes() {
        let tests = vec![
            test_named("ok-1", 200),
            test_named("fail-1", 500),
            test_named("ok-2", 200),
            test_named("fail-2", 404),
        ];

        let (warmup, pool) = select_pool(&tests, 16, 42);

        let warmup_names: Vec<&str> = warmup.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(warmup_names, vec!["fail-1", "fail-2"]);
        // The requested pool size is honored by cycling the 2 eligible tests.
        assert_eq!(pool.len(), 16);
        assert!(pool.iter().all(|t| t.expected_status == 200));
        let mut names: Vec<&str> = pool.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names, vec!["ok-1", "ok-2"]);
    }

    #[test]
    fn for_each_tests_are_excluded_everywhere() {
        let tests = vec![test_named("ok", 200), looped("loop")];
        let (warmup, pool) = select_pool(&tests, 16, 42);
        assert!(warmup.is_empty());
        // The looped test never enters the pool, even through cycling.
        assert_eq!(pool.len(), 16);
        assert!(pool.iter().all(|t| t.name == "ok"));
    }

    #[test]
    fn pool_is_capped_without_duplicates() {
        let tests: Vec<TestRequest> = (0..40).map(|i| test_named(&format!("t{i}"), 200)).collect();
        let (_, pool) = select_pool(&tests, 16, 7);

        assert_eq!(pool.len(), 16);
        let mut names: Vec<&str> = pool.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 16, "sampling must be without replacement");
    }

    #[test]
    fn pool_larger_than_eligible_cycles_tests_evenly() {
        use std::collections::HashMap;

        let tests: Vec<TestRequest> = (0..5).map(|i| test_named(&format!("t{i}"), 200)).collect();
        let (_, pool) = select_pool(&tests, 10, 7);

        assert_eq!(pool.len(), 10, "the requested pool size must be honored");
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for test in &pool {
            *counts.entry(test.name.as_str()).or_default() += 1;
        }
        assert_eq!(counts.len(), 5, "every eligible test takes part");
        assert!(
            counts.values().all(|&c| c == 2),
            "5 tests cycled into a pool of 10 -> each exactly twice: {counts:?}"
        );
    }

    #[test]
    fn pool_cycling_with_a_non_multiple_size_stays_balanced() {
        use std::collections::HashMap;

        let tests: Vec<TestRequest> = (0..4).map(|i| test_named(&format!("t{i}"), 200)).collect();
        let (_, pool) = select_pool(&tests, 10, 7);

        assert_eq!(pool.len(), 10);
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for test in &pool {
            *counts.entry(test.name.as_str()).or_default() += 1;
        }
        assert_eq!(counts.len(), 4, "every eligible test takes part");
        assert!(
            counts.values().all(|&c| c == 2 || c == 3),
            "10 over 4 tests -> two or three runs each: {counts:?}"
        );
    }

    #[test]
    fn sampling_is_deterministic_for_a_seed_and_varies_across_seeds() {
        let tests: Vec<TestRequest> = (0..40).map(|i| test_named(&format!("t{i}"), 200)).collect();

        let names = |seed| {
            let (_, pool) = select_pool(&tests, 16, seed);
            pool.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
        };

        assert_eq!(names(7), names(7), "same seed, same sample");
        assert_ne!(names(7), names(8), "different seeds should differ");
    }

    #[test]
    fn pool_order_is_pinned_for_a_seed() {
        // Characterization guard: the exact pool order for a fixed seed is
        // locked in, so any change to sampling that alters its output -- such
        // as the index-shuffle refactor -- is caught. Values are the xorshift64
        // shuffle of t0..t7 seeded with 42.
        let tests: Vec<TestRequest> = (0..8).map(|i| test_named(&format!("t{i}"), 200)).collect();
        let (_, pool) = select_pool(&tests, 8, 42);
        let names: Vec<&str> = pool.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["t2", "t4", "t0", "t1", "t6", "t5", "t7", "t3"]);

        // Same seed, but a pool larger than the eligible set: the pinned order
        // is cycled to fill the extra slots.
        let three: Vec<TestRequest> = (0..3).map(|i| test_named(&format!("t{i}"), 200)).collect();
        let (_, cycled) = select_pool(&three, 8, 42);
        let cycled_names: Vec<&str> = cycled.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            cycled_names,
            ["t1", "t2", "t0", "t1", "t2", "t0", "t1", "t2"]
        );
    }

    // ---------- Server-Timing ----------

    fn headers_with(value: &str) -> HashMap<String, String> {
        HashMap::from([("server-timing".to_string(), value.to_string())])
    }

    #[test]
    fn parses_a_single_server_timing_metric() {
        let timing = parse_server_timing(&headers_with("app;dur=42.5")).unwrap();
        assert_eq!(timing.server_ms, 42.5);
        assert!(timing.parts.is_empty());
    }

    #[test]
    fn total_metric_is_preferred_and_components_become_parts() {
        let timing =
            parse_server_timing(&headers_with("total;dur=27.1706,app;dur=10.2,db;dur=16.9"))
                .unwrap();
        assert_eq!(timing.server_ms, 27.1706);
        assert_eq!(
            timing.parts,
            vec![
                ServerSpan {
                    name: "app".to_string(),
                    dur_ms: 10.2
                },
                ServerSpan {
                    name: "db".to_string(),
                    dur_ms: 16.9
                },
            ]
        );
    }

    #[test]
    fn total_wins_even_over_a_larger_component() {
        let timing = parse_server_timing(&headers_with("app;dur=99, total;dur=27")).unwrap();
        assert_eq!(timing.server_ms, 27.0);
    }

    #[test]
    fn total_name_is_case_insensitive() {
        let timing = parse_server_timing(&headers_with("Total;DUR=7")).unwrap();
        assert_eq!(timing.server_ms, 7.0);
    }

    #[test]
    fn without_total_the_largest_span_wins_and_parts_stay_empty() {
        // Without an explicit total we cannot know whether spans nest or
        // follow each other; the dominant one is the safest estimate and
        // sub-spans would be misleading.
        let timing = parse_server_timing(&headers_with("db;dur=10.2, app;dur=42.5")).unwrap();
        assert_eq!(timing.server_ms, 42.5);
        assert!(timing.parts.is_empty());
    }

    #[test]
    fn server_timing_without_dur_or_header_is_none() {
        assert!(parse_server_timing(&headers_with("missedCache")).is_none());
        assert!(parse_server_timing(&headers_with("app;dur=abc")).is_none());
        assert!(parse_server_timing(&HashMap::new()).is_none());
    }

    // ---------- latency probe ----------

    #[tokio::test]
    async fn http_probe_gives_up_after_the_first_timeout() {
        use tokio::net::TcpListener;

        // Black hole: accepts connections but never answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    drop(socket);
                });
            }
        });

        let client = reqwest::Client::new();
        let started = Instant::now();
        let samples = measure_http(
            &client,
            &format!("http://{addr}/probe"),
            Duration::from_millis(80),
        )
        .await;

        assert!(samples.is_empty(), "no answer -> no sample");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the probe must give up on the first timeout instead of \
             timing out {LATENCY_PROBES} times"
        );
    }

    #[test]
    fn probe_target_parses_host_port_and_scheme_defaults() {
        assert_eq!(
            probe_target("https://api.example.com/path?x=1"),
            Some(("api.example.com".to_string(), 443))
        );
        assert_eq!(
            probe_target("http://api.example.com/path"),
            Some(("api.example.com".to_string(), 80))
        );
        assert_eq!(
            probe_target("http://127.0.0.1:8043/price"),
            Some(("127.0.0.1".to_string(), 8043))
        );
        assert_eq!(probe_target("not a url"), None);
    }
}
