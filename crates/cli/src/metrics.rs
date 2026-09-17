//! Opt-in per-step timing metrics (`--metrics`): a tracing [`Layer`] that
//! aggregates span durations by span name, rendered as a summary table at
//! the end of the run.
//!
//! A span's duration is its full lifetime (creation to close), so awaiting
//! inside a span counts as time spent in that step - which is the question
//! the table answers: where does the wall-clock time of a run go? Under
//! concurrency, steps overlap, so the totals can legitimately exceed the
//! run's wall time.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::span;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use vantage_core::stats::{calculate_median, calculate_p90};

/// Aggregated span durations, in microseconds, keyed by span name.
#[derive(Debug, Default)]
pub struct StepTimings {
    durations: Mutex<HashMap<&'static str, Vec<u128>>>,
}

/// One step's aggregated timings, in microseconds.
struct StepAggregate {
    name: &'static str,
    count: usize,
    total_us: u128,
    median_us: u128,
    p90_us: u128,
}

impl StepTimings {
    fn record(&self, name: &'static str, elapsed_us: u128) {
        if let Ok(mut durations) = self.durations.lock() {
            durations.entry(name).or_default().push(elapsed_us);
        }
    }

    /// Aggregates the recorded samples per step, ordered by total time
    /// spent, descending. Shared by the console table and the report JSON.
    fn aggregates(&self) -> Vec<StepAggregate> {
        let Ok(durations) = self.durations.lock() else {
            return Vec::new();
        };

        let mut rows: Vec<StepAggregate> = durations
            .iter()
            .map(|(name, samples)| {
                let mut sorted = samples.clone();
                sorted.sort_unstable();
                StepAggregate {
                    name,
                    count: sorted.len(),
                    total_us: sorted.iter().sum(),
                    median_us: calculate_median(&sorted),
                    p90_us: calculate_p90(&sorted),
                }
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.total_us));
        rows
    }

    /// Renders the summary table, steps sorted by total time spent.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from(
            "\nMETRIQUES PAR ETAPE (--metrics)\n\
             Etape           n      Total   Mediane       P90\n",
        );
        for row in self.aggregates() {
            let _ = writeln!(
                out,
                "{:<12} {:>5} {:>9.1}ms {:>7.1}ms {:>7.1}ms",
                row.name,
                row.count,
                row.total_us as f64 / 1000.0,
                row.median_us as f64 / 1000.0,
                row.p90_us as f64 / 1000.0,
            );
        }
        out
    }

    /// The same per-step aggregates as [`render`](Self::render), as JSON for
    /// embedding in an HTML report: each step carries its measured stats plus a
    /// human description and expected per-call budget (see [`step_meta`]).
    /// Steps are ordered by total time spent, descending.
    #[must_use]
    pub fn as_json(&self) -> serde_json::Value {
        let steps: Vec<serde_json::Value> = self
            .aggregates()
            .into_iter()
            .map(|row| {
                let (description, expected_ms) = step_meta(row.name);
                serde_json::json!({
                    "name": row.name,
                    "count": row.count,
                    "total_ms": row.total_us as f64 / 1000.0,
                    "median_ms": row.median_us as f64 / 1000.0,
                    "p90_ms": row.p90_us as f64 / 1000.0,
                    "expected_ms": expected_ms,
                    "description": description,
                })
            })
            .collect();
        serde_json::json!(steps)
    }

    #[cfg(test)]
    fn count(&self, name: &str) -> usize {
        self.durations
            .lock()
            .map(|d| d.get(name).map_or(0, Vec::len))
            .unwrap_or(0)
    }
}

/// Explanation and expected per-call duration (ms) for each known step, shown
/// in the report's `(i)` tooltips. `None` means the step has no fixed budget
/// (its cost is inherently variable, e.g. user-defined action hooks).
fn step_meta(name: &str) -> (&'static str, Option<f64>) {
    match name {
        "load_suite" => ("Reading and parsing the suite JSON from disk.", Some(5.0)),
        "prepare" => (
            "Building the request: resolving {{templates}} in the URL, payload and headers.",
            Some(1.0),
        ),
        "http" => (
            "Network round-trip: sending the request and receiving the response (server \
             processing + latency). Normally the largest step.",
            Some(250.0),
        ),
        "grade" => (
            "Comparing the response against expected_response, after ignored_fields and sorts.",
            Some(5.0),
        ),
        "extract" => (
            "Applying capture pointers to pull values (e.g. the auth token) into the dictionary.",
            Some(1.0),
        ),
        "hooks" => (
            "Running before/after CLI action hooks (shell commands); the cost is whatever they do.",
            None,
        ),
        "report" => ("Writing the HTML and data.json report.", Some(50.0)),
        _ => ("", None),
    }
}

/// Start-of-life marker stored in each span's extensions.
struct StartTime(Instant);

/// Records every span's lifetime into a shared [`StepTimings`].
pub struct TimingLayer {
    timings: Arc<StepTimings>,
}

impl<S> Layer<S> for TimingLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(StartTime(Instant::now()));
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id)
            && let Some(start) = span.extensions().get::<StartTime>()
        {
            self.timings
                .record(span.metadata().name(), start.0.elapsed().as_micros());
        }
    }
}

/// Installs the metrics subscriber globally; call once, before any span
/// fires. Returns the shared timings to render at the end of the run.
pub fn install() -> Arc<StepTimings> {
    let timings = Arc::new(StepTimings::default());
    tracing_subscriber::registry()
        .with(TimingLayer {
            timings: timings.clone(),
        })
        .init();
    timings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_layer(f: impl FnOnce()) -> Arc<StepTimings> {
        let timings = Arc::new(StepTimings::default());
        let subscriber = tracing_subscriber::registry().with(TimingLayer {
            timings: timings.clone(),
        });
        tracing::subscriber::with_default(subscriber, f);
        timings
    }

    #[test]
    fn aggregates_span_lifetimes_by_name() {
        let timings = with_layer(|| {
            for _ in 0..3 {
                let span = tracing::info_span!("http");
                let _entered = span.enter();
            }
            let span = tracing::info_span!("grade");
            let _entered = span.enter();
        });

        assert_eq!(timings.count("http"), 3);
        assert_eq!(timings.count("grade"), 1);
        assert_eq!(timings.count("nope"), 0);
    }

    #[test]
    fn render_lists_every_step_with_its_count() {
        let timings = with_layer(|| {
            for _ in 0..2 {
                let span = tracing::info_span!("extract");
                let _entered = span.enter();
            }
        });

        let table = timings.render();
        assert!(table.contains("extract"), "{table}");
        assert!(table.contains("Mediane"), "{table}");
    }

    #[test]
    fn render_without_any_span_is_just_the_header() {
        let timings = StepTimings::default();
        let table = timings.render();
        assert!(table.contains("METRIQUES"), "{table}");
    }

    #[test]
    fn as_json_carries_stats_description_and_expected() {
        let timings = with_layer(|| {
            for _ in 0..2 {
                let span = tracing::info_span!("http");
                let _entered = span.enter();
            }
        });

        let json = timings.as_json();
        let steps = json.as_array().expect("as_json returns an array");
        assert_eq!(steps.len(), 1);
        let http = &steps[0];
        assert_eq!(http["name"], "http");
        assert_eq!(http["count"], 2);
        assert_eq!(http["expected_ms"], 250.0);
        assert!(
            http["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Network"),
            "http step carries its explanation"
        );
    }

    #[test]
    fn as_json_of_empty_timings_is_an_empty_array() {
        assert_eq!(StepTimings::default().as_json(), serde_json::json!([]));
    }
}
