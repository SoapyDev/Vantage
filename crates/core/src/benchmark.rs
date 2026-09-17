//! Benchmark report data types.
//!
//! Produced by the runner's `--benchmark` engine and rendered by the
//! `reporter` crate. Kept here (like [`TestResult`](crate::result::TestResult))
//! so the reporter can render them without depending on the execution engine.

use crate::load::{LoadCurve, LoadProfile, Stage};
use serde::Serialize;
use std::time::Duration;

/// Latency probe samples, in microseconds (latencies are often sub-ms, where
/// the millisecond granularity used elsewhere would round to 0).
#[derive(Debug, Serialize)]
pub struct LatencyReport {
    /// Raw TCP connect times: the network round-trip floor.
    pub tcp_us: Vec<u128>,
    /// Minimal HTTP round-trips: the floor including TLS/proxy/framework.
    pub http_us: Vec<u128>,
}

impl LatencyReport {
    /// Median TCP connect time, in milliseconds (0 when no sample).
    #[must_use]
    pub fn tcp_median_ms(&self) -> f64 {
        median_ms(&self.tcp_us)
    }

    /// Median HTTP round-trip time, in milliseconds (0 when no sample).
    #[must_use]
    pub fn http_median_ms(&self) -> f64 {
        median_ms(&self.http_us)
    }
}

/// Median of unsorted microsecond samples, in milliseconds; 0 when empty.
fn median_ms(samples_us: &[u128]) -> f64 {
    if samples_us.is_empty() {
        return 0.0;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    crate::stats::calculate_median(&sorted) as f64 / 1000.0
}

/// True when a count is zero; lets zero-valued fields drop out of a report.
fn is_zero(value: &u128) -> bool {
    *value == 0
}

/// One request observation. Body and headers are intentionally not kept.
#[derive(Debug, Clone, Serialize)]
pub struct BenchSample {
    /// Display name of the pooled test this call executed.
    pub name: String,
    /// Offset of the call's dispatch from the load-profile start, in
    /// milliseconds. Zero (and omitted from the report) for escalation-phase
    /// samples, which have no profile timeline.
    #[serde(skip_serializing_if = "is_zero")]
    pub offset_ms: u128,
    /// Wall-clock duration of the call, in milliseconds.
    pub duration_ms: u128,
    /// HTTP status received, if the call completed.
    pub status: Option<u16>,
    /// Whether the status matched the test's expected status.
    pub is_success: bool,
    /// Server-reported processing time, from an optional `Server-Timing`
    /// response header. When present, reports can split the true per-call
    /// latency (`duration_ms - server_ms`) from the processing time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_ms: Option<f64>,
    /// Named sub-spans within `server_ms` (the other `Server-Timing`
    /// metrics when an explicit `total` exists), for the per-call graph.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub server_parts: Vec<ServerSpan>,
}

/// One named `Server-Timing` component (e.g. `db;dur=16.9`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerSpan {
    /// Metric name, as reported by the server.
    pub name: String,
    /// Reported duration, in milliseconds.
    pub dur_ms: f64,
}

/// All observations for one concurrency level.
#[derive(Debug, Serialize)]
pub struct PhaseReport {
    /// Requests in flight during the phase (1 = sequential baseline).
    pub concurrency: usize,
    /// Wall-clock duration of the whole phase, in milliseconds.
    pub wall_ms: u128,
    /// One observation per pooled request.
    pub samples: Vec<BenchSample>,
}

impl PhaseReport {
    /// Number of failed samples.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.samples.iter().filter(|s| !s.is_success).count()
    }

    /// Failed fraction of the samples (0 when the phase is empty).
    #[must_use]
    pub fn error_rate(&self) -> f64 {
        if self.samples.is_empty() {
            0.0
        } else {
            self.error_count() as f64 / self.samples.len() as f64
        }
    }

    /// Whether any sample hit HTTP 429 (rate limiting).
    #[must_use]
    pub fn rate_limited(&self) -> bool {
        self.samples.iter().any(|s| s.status == Some(429))
    }

    /// The sample durations sorted ascending, ready for percentile math.
    #[must_use]
    pub fn sorted_durations(&self) -> Vec<u128> {
        let mut durations: Vec<u128> = self.samples.iter().map(|s| s.duration_ms).collect();
        durations.sort_unstable();
        durations
    }

    /// Throughput of the phase (0 when the wall time is 0).
    #[must_use]
    pub fn requests_per_second(&self) -> f64 {
        if self.wall_ms == 0 {
            0.0
        } else {
            self.samples.len() as f64 * 1000.0 / self.wall_ms as f64
        }
    }
}

/// Final benchmark outcome.
#[derive(Debug, Serialize)]
pub struct BenchReport {
    /// Expected-fail tests fired once, sequentially, before measuring.
    pub warmup_count: usize,
    /// Number of pooled requests run by each phase.
    pub pool_size: usize,
    /// Latency probe (phase 0); `None` when the suite URL could not be
    /// parsed into a probe target.
    pub latency: Option<LatencyReport>,
    /// The measured phases, sequential baseline first.
    pub phases: Vec<PhaseReport>,
    /// Why the escalation stopped early, if it did.
    pub stop_reason: Option<String>,
    /// The load-profile portion of the run, when `--load` was used; `None`
    /// for a pure escalation benchmark.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load: Option<LoadReport>,
}

/// One time bucket (one second) of the load-profile timeline, aggregating the
/// samples whose dispatch (`sent`) or completion (`completed`, `errors`, the
/// percentiles) falls within it.
///
/// Bucketing dispatch and completion separately is what makes coordinated
/// omission visible: a request sent near the end of one second and answered in
/// the next is counted as `sent` in the first bucket and `completed` in the
/// second, so a widening gap between the two lines flags saturation.
#[derive(Debug, Serialize)]
pub struct TimeBucket {
    /// Bucket start offset from the profile start, in whole seconds.
    pub second: u64,
    /// Target rate the curve prescribed across the bucket (calls per second),
    /// sampled at the bucket midpoint.
    pub target_cps: f64,
    /// Requests dispatched during the bucket.
    pub sent: usize,
    /// Requests that completed during the bucket.
    pub completed: usize,
    /// Failed completions during the bucket.
    pub errors: usize,
    /// Median duration of the calls completed in the bucket, in milliseconds.
    pub p50_ms: u128,
    /// P90 duration of the calls completed in the bucket, in milliseconds.
    pub p90_ms: u128,
    /// P99 duration of the calls completed in the bucket, in milliseconds.
    pub p99_ms: u128,
}

/// The load-profile portion of a benchmark run: the stages that ran, their
/// per-second aggregation, every raw sample (with its offset) for external
/// exploitation, and why the run stopped early if it did.
#[derive(Debug, Serialize)]
pub struct LoadReport {
    /// The stages that were run, echoed for the report graphs.
    pub stages: Vec<Stage>,
    /// Per-second aggregation of the run.
    pub buckets: Vec<TimeBucket>,
    /// Every raw sample, each carrying its dispatch offset.
    pub samples: Vec<BenchSample>,
    /// Why the run stopped early (adaptive stop), if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

impl LoadReport {
    /// Builds a report from the profile that was run and the samples it
    /// produced, aggregating them into per-second [`TimeBucket`]s.
    #[must_use]
    pub fn new(
        profile: &LoadProfile,
        samples: Vec<BenchSample>,
        stop_reason: Option<String>,
    ) -> Self {
        let buckets = aggregate_buckets(&profile.compile(), &samples);
        Self {
            stages: profile.stages.clone(),
            buckets,
            samples,
            stop_reason,
        }
    }
}

/// Aggregates raw samples into one-second [`TimeBucket`]s spanning both the
/// profile duration and the last completion, so the target-rate line is drawn
/// for the whole profile even where no traffic landed.
fn aggregate_buckets(curve: &LoadCurve, samples: &[BenchSample]) -> Vec<TimeBucket> {
    let profile_secs = curve.total_duration().as_secs_f64().ceil() as u64;
    let last_completion = samples
        .iter()
        .map(|s| ((s.offset_ms + s.duration_ms) / 1000) as u64)
        .max();
    let bucket_count = match last_completion {
        Some(last) => profile_secs.max(last + 1),
        None => profile_secs,
    };
    if bucket_count == 0 {
        return Vec::new();
    }

    let n = bucket_count as usize;
    let mut sent = vec![0usize; n];
    let mut completed = vec![0usize; n];
    let mut errors = vec![0usize; n];
    let mut durations: Vec<Vec<u128>> = vec![Vec::new(); n];

    for sample in samples {
        // Indices are within range by construction, but clamp defensively so a
        // rounding edge can never panic.
        let send = ((sample.offset_ms / 1000) as usize).min(n - 1);
        let done = (((sample.offset_ms + sample.duration_ms) / 1000) as usize).min(n - 1);
        sent[send] += 1;
        completed[done] += 1;
        if !sample.is_success {
            errors[done] += 1;
        }
        durations[done].push(sample.duration_ms);
    }

    (0..n)
        .map(|i| {
            let mut sorted = std::mem::take(&mut durations[i]);
            sorted.sort_unstable();
            let (p50_ms, p90_ms, p99_ms) = if sorted.is_empty() {
                (0, 0, 0)
            } else {
                (
                    crate::stats::calculate_median(&sorted),
                    crate::stats::calculate_p90(&sorted),
                    crate::stats::calculate_p99(&sorted),
                )
            };
            TimeBucket {
                second: i as u64,
                // Midpoint of the second: for a linear ramp this is exactly the
                // average target rate over the bucket.
                target_cps: curve.cps_at(Duration::from_secs_f64(i as f64 + 0.5)),
                sent: sent[i],
                completed: completed[i],
                errors: errors[i],
                p50_ms,
                p90_ms,
                p99_ms,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(duration_ms: u128, success: bool) -> BenchSample {
        offset_sample(0, duration_ms, success)
    }

    fn offset_sample(offset_ms: u128, duration_ms: u128, success: bool) -> BenchSample {
        BenchSample {
            name: "call".to_string(),
            offset_ms,
            duration_ms,
            status: Some(if success { 200 } else { 500 }),
            is_success: success,
            server_ms: None,
            server_parts: vec![],
        }
    }

    #[test]
    fn latency_report_medians_are_in_milliseconds() {
        let report = LatencyReport {
            tcp_us: vec![2100, 1900, 2000],
            http_us: vec![8400, 8600, 8500],
        };
        assert!((report.tcp_median_ms() - 2.0).abs() < 1e-9);
        assert!((report.http_median_ms() - 8.5).abs() < 1e-9);
    }

    #[test]
    fn latency_report_medians_of_empty_samples_are_zero() {
        let report = LatencyReport {
            tcp_us: vec![],
            http_us: vec![],
        };
        assert_eq!(report.tcp_median_ms(), 0.0);
        assert_eq!(report.http_median_ms(), 0.0);
    }

    #[test]
    fn phase_report_statistics() {
        let phase = PhaseReport {
            concurrency: 2,
            wall_ms: 200,
            samples: vec![sample(100, true), sample(120, true), sample(90, false)],
        };
        assert_eq!(phase.error_count(), 1);
        assert!((phase.error_rate() - 1.0 / 3.0).abs() < 1e-9);
        assert!(!phase.rate_limited());
        assert_eq!(phase.sorted_durations(), vec![90, 100, 120]);
        assert!((phase.requests_per_second() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn rate_limited_detects_a_429() {
        let mut s = sample(50, false);
        s.status = Some(429);
        let phase = PhaseReport {
            concurrency: 1,
            wall_ms: 50,
            samples: vec![s],
        };
        assert!(phase.rate_limited());
    }

    // ---------- load report: offset_ms serialization ----------

    #[test]
    fn offset_ms_is_omitted_when_zero_and_kept_otherwise() {
        let escalation = serde_json::to_value(sample(100, true)).unwrap();
        assert!(
            escalation.get("offset_ms").is_none(),
            "escalation samples must not carry an offset"
        );

        let profiled = serde_json::to_value(offset_sample(1500, 100, true)).unwrap();
        assert_eq!(
            profiled.get("offset_ms").and_then(|v| v.as_u64()),
            Some(1500)
        );
    }

    // ---------- load report: bucket aggregation ----------

    #[test]
    fn buckets_split_dispatch_from_completion() {
        let profile = LoadProfile::parse("step:3s:10").unwrap();
        let samples = vec![
            offset_sample(0, 500, true),    // sent b0, done b0
            offset_sample(200, 1200, true), // sent b0, done b1 (1.4s)
            offset_sample(1000, 100, true), // sent b1, done b1
        ];
        let report = LoadReport::new(&profile, samples, None);
        let buckets = &report.buckets;
        assert_eq!(buckets.len(), 3);

        assert_eq!((buckets[0].sent, buckets[0].completed), (2, 1));
        assert_eq!((buckets[1].sent, buckets[1].completed), (1, 2));
        assert_eq!((buckets[2].sent, buckets[2].completed), (0, 0));

        // Every stage second targets the constant 10 CPS.
        for bucket in buckets {
            assert!((bucket.target_cps - 10.0).abs() < 1e-9);
        }

        // b0 completed [500] -> median 500; b1 completed [100, 1200] -> 650.
        assert_eq!(buckets[0].p50_ms, 500);
        assert_eq!(buckets[1].p50_ms, 650);
    }

    #[test]
    fn errors_are_counted_in_the_completion_bucket() {
        let profile = LoadProfile::parse("step:2s:5").unwrap();
        let samples = vec![offset_sample(500, 100, false), offset_sample(600, 50, true)];
        let report = LoadReport::new(&profile, samples, Some("HTTP 429".to_string()));
        assert_eq!(report.buckets[0].errors, 1);
        assert_eq!(report.buckets[0].completed, 2);
        assert_eq!(report.stop_reason.as_deref(), Some("HTTP 429"));
    }

    #[test]
    fn empty_run_still_spans_the_profile_with_the_target_curve() {
        // A 0->8 ramp over 4s, no traffic: one bucket per second, target only.
        let profile = LoadProfile::parse("ramp:4s:8").unwrap();
        let report = LoadReport::new(&profile, vec![], None);
        assert_eq!(report.buckets.len(), 4);
        let targets: Vec<f64> = report.buckets.iter().map(|b| b.target_cps).collect();
        // Midpoints 0.5,1.5,2.5,3.5 on a 0->8 ramp over 4s -> 1,3,5,7.
        for (bucket, expected) in report.buckets.iter().zip([1.0, 3.0, 5.0, 7.0]) {
            assert!((bucket.target_cps - expected).abs() < 1e-9, "{targets:?}");
            assert_eq!((bucket.sent, bucket.completed), (0, 0));
        }
        assert_eq!(report.stages.len(), 1);
    }
}
