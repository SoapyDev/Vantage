# Roadmap

## 1. Load profiles — curve-driven benchmarks (approved)

### Motivation

`--benchmark` is a *closed-loop* probe: it escalates concurrency (1, 2, 4, … 128)
and each phase fires the pool as fast as the machine allows. That answers
"where is the ceiling?" but not "how does the service behave under a
*controlled, realistic* load over time?". Load profiles add an *open-loop*
mode: requests are scheduled at a target rate (calls per second) that follows
a curve over a wall-clock duration, independently of how fast responses come
back.

Example: a linear ramp over 3 min up to 30 CPS, then a plateau held for
2 min — 5 minutes total.

### Proposed UX

New `--load` flag, an optional addition to `--benchmark` (requires it). The
classic concurrency escalation stays the default and gets its own toggle:

```
# today's behavior, unchanged: escalation only
vantage -f suite.json --benchmark

# escalation, then the load profile (custom curve: comma-separated stages)
vantage -f suite.json --benchmark --load "ramp:3m:30,hold:2m"

# load profile only, escalation disabled (preset)
vantage -f suite.json --benchmark --load spike --no-escalation
```

| Flags | Behavior |
|-------|----------|
| `--benchmark` | escalation phases (current behavior) |
| `--benchmark --load <p>` | escalation phases, then the load profile |
| `--benchmark --load <p> --no-escalation` | load profile only |
| `--benchmark --no-escalation` | rejected (nothing left to run) |

Shared protocol either way: steps (auth), latency probes, warm-up and pool
selection run once, up front. Escalation results also give the report context
to annotate the profile graphs (e.g. the known concurrency ceiling).

Stage grammar: `<shape>:<duration>[:<target_cps>[:<period>]]`

| Shape   | Meaning                                                        |
|---------|----------------------------------------------------------------|
| `ramp`  | linear from the previous stage's CPS (0 initially) to `target` — down as well as up (`ramp:3m:30,hold:30s,ramp:3m:0` is a valid rise/hold/descent) |
| `hold`  | constant at the previous stage's CPS (`target` optional)       |
| `step`  | jump instantly to `target`, hold for `duration`                |
| `sine`  | oscillate between the previous stage's CPS and `target`, one full wave per `period` (e.g. `sine:5m:30:1m`); integral is closed-form, so it costs the scheduler nothing |

Presets (built from the same grammar, so they double as documentation):

| Preset   | Expansion                                  | Use case                |
|----------|--------------------------------------------|-------------------------|
| `smoke`  | `hold:30s:1`                               | sanity at trivial load  |
| `ramp`   | `ramp:3m:30,hold:2m`                       | find degradation point  |
| `spike`  | `hold:1m:5,step:30s:50,hold:1m:5`          | burst recovery          |
| `stairs` | `step:1m:5,step:1m:10,step:1m:20,step:1m:40` | capacity in increments |
| `soak`   | `ramp:1m:10,hold:10m`                      | sustained-load leaks    |

The same profile is also declarable in the suite file (CLI wins on conflict):

```json
{
  "load": {
    "stages": [
      { "shape": "ramp", "duration": "3m", "target_cps": 30 },
      { "shape": "hold", "duration": "2m" }
    ]
  }
}
```

Safety flags: `--max-in-flight <n>` (default 256) caps concurrent requests so
a slow server cannot pile up unbounded tasks; the existing adaptive stop
(HTTP 429, >20 % error rate — evaluated per time bucket) applies, with
`--no-adaptive-stop` to opt out against services known to rate-limit.

### Report: two time-series graphs

The benchmark report keeps its current sections (latency probe, per-phase
bars); when `--load` ran, a load-profile section is appended. Rendered in the
same self-contained offline HTML (inline SVG, no external dependency):

1. **Throughput over time** — target CPS curve overlaid with achieved CPS
   (per-second buckets). The gap between the two lines makes saturation and
   client-side ceilings immediately visible.
2. **Response time over time** — per-bucket median line with a p90–p99 band;
   error samples flagged in red. Server-Timing split reused where present.

`data.json` keeps every raw sample (with its time offset) for external
exploitation.

### Feasibility

Feasible with the existing stack — no new dependency.

- **Scheduler**: open-loop arrival scheduling on tokio. Compute the expected
  cumulative arrivals `N(t) = ∫ cps(t) dt` (piecewise-linear, closed form per
  stage) and fire whenever the sent count falls below `N(elapsed)`, on a
  coarse tick (~20 ms). Budget-based scheduling avoids per-request sleeps and
  OS timer-resolution issues. `JoinSet` + a semaphore (as the in-flight cap,
  not a rate driver) are already the house pattern in `runner`.
- **Request source**: `select_pool` is reused as-is; the scheduler draws from
  the shuffled eligible pool round-robin. Steps (auth), warm-up and latency
  probes run unchanged before the profile starts. `for_each` tests stay
  excluded.
- **Data model**: `BenchSample` gains an `offset_ms` (start time relative to
  the profile start). `BenchReport` gains an optional
  `load: Option<LoadReport>` — one report covers both modes. New
  `LoadReport { stages, buckets, samples, stop_reason }` with per-second
  `TimeBucket { target_cps, sent, completed, errors, p50/p90/p99 }` in
  `core::benchmark`, keeping the reporter decoupled from the engine.
- **Testing**: `mock-server` already supports configurable delays and hit
  counts — enough to assert the achieved rate tracks the curve and that the
  in-flight cap and adaptive stop engage. Curve math (`cps_at(t)`, cumulative
  arrivals, stage parsing) is pure and unit-testable in `core`.

Known risks, handled by design:

- **Client-side ceiling**: the local machine / connection pool may not reach
  high CPS targets; plotting target vs achieved makes this visible instead of
  silently lying.
- **Rate-limited targets**: adaptive stop stays on by default, consistent
  with the bounded-budget philosophy of `--benchmark`.
- **Coordinated omission**: because scheduling is open-loop, slow responses
  do not slow the arrival rate; when the in-flight cap is hit, skipped
  arrivals are counted and reported rather than hidden.

### Work breakdown

| # | Item | Crate | Size |
|---|------|-------|------|
| 1 | `LoadProfile` / `Stage` types, spec-string + JSON parsing, presets, `cps_at(t)` and cumulative-arrivals math + unit tests | `core` | M |
| 2 | `offset_ms` on `BenchSample`; `LoadReport`, `TimeBucket` + bucket aggregation | `core` | S |
| 3 | Open-loop scheduler: tick loop, round-robin pool draw, in-flight cap, per-bucket adaptive stop; wire into `run_benchmark` after the escalation phases (or instead of, with `--no-escalation`); integration tests against `mock-server` | `runner` | L |
| 4 | `--load`, `--no-escalation`, `--max-in-flight`, `--no-adaptive-stop` flags; validation (`--load`/`--no-escalation` require `--benchmark`; reject `--no-escalation` without `--load`); group-profile support | `cli` | S |
| 5 | Time-series SVG line-chart helper; throughput + response-time graphs appended to the benchmark report; summary table; `data.json` | `reporter` | M |
| 6 | Console summary (per-stage roll-up) + README section + example suite | `cli`/docs | S |

Suggested order: 1 → 2 → 3 → 4 → 5 → 6 (3 is the core of the feature; 4–5 can
proceed in parallel once 2 lands).

### Open questions

- Should a run *fail* (non-zero exit) on adaptive stop, or only report it?
  (Proposal: report only, add `--fail-on-stop` later if CI needs it.)
- Sub-1 CPS rates (e.g. one call every 10 s for soak tests): supported by the
  cumulative-arrivals math for free — worth exposing as fractional targets?

---

## 2. Approved features

Evaluated against the current codebase; none started. Suggested order:
2.4 (quick win) → 2.8 → 2.2/2.3 → 2.1 → 2.6 → 2.7 → 2.5.

### 2.1 JSON Schema validation

Structural checks instead of exact values.

- **Today**: the comparator only does a strict full-body match
  (`assert_json_diff`, extra fields fail); `expected_response: null` skips
  the body entirely. Nothing in between.
- **Design**: new **optional** per-request field `expected_schema` — omitted,
  behavior is exactly today's. Value is an inline JSON Schema or a path to a
  `.schema.json` file (resolved relative to the suite file).
  Validated with the `jsonschema` crate (draft 2020-12). Combinable with
  `expected_response` (both must pass). Violations reported with their JSON
  pointer + rule, in console and HTML.
- **Work**: `core` — field on `TestRequest` (+ suite-level default);
  `runner` — comparator branch + schema-file loading/caching (the dependency
  stays out of `core`, which remains dependency-free); `reporter` —
  violations list rendering. New dep: `jsonschema`. **Size: M.**
- **Note**: in compare mode the schema applies to each side independently.

### 2.2 `multipart/form-data`

- **Today**: declared in `HttpContentType` and the reqwest `multipart`
  feature is already enabled, but `make_request` rejects it
  (`runner/src/lib.rs:99`).
- **Design**: payload convention — an object whose values are either strings
  (text parts) or `{ "file": "path", "filename"?, "mime"? }` (file parts).
  Templates injected first, as today; file paths resolved relative to the
  suite file; bytes read synchronously into `reqwest::multipart::Part`.
  `--dry-run` lists the parts without reading files.
- **Work**: `runner` — build the `Form` in `make_request` (currently sync;
  file reads via `std::fs` keep it that way); `core` — none (payload is
  already an arbitrary `Value`); docs + example suite. **Size: M.**

### 2.3 Non-JSON response bodies

- **Today**: `record_response` keeps the body only when `response.json()`
  succeeds — text/XML/HTML responses are silently dropped, so they can be
  neither asserted nor captured.
- **Design**: read the bytes once, then: valid JSON → current path; valid
  UTF-8 → stored as a string body (literal comparison against a string
  `expected_response`, capture with the empty pointer works); binary →
  store length + SHA-256 only, with a size cap on stored text. `TestResult`
  gains a body-kind tag so the reporter can render text diffs vs JSON diffs.
- **Work**: `runner` — `record_response` + comparator string branch;
  `core` — `TestResult` body kind; `reporter` — text-diff rendering.
  **Size: M.**

### 2.4 YAML test suites

- **Today**: the loader accepts `.json` only (`loader.rs:19`); YAML is
  groups-only, though serde-YAML support is already in the tree.
- **Design**: branch on extension, deserialize into the *same* `TestSuite`
  serde model — no schema divergence, JSON stays canonical. Free wins: real
  comments (replacing the `_note` convention) and YAML anchors for repeated
  payload fragments.
- **Work**: `cli` loader + tests + README. No new dependency. **Size: S** —
  the cheapest item; do it first.

### 2.5 OpenAPI import

- **Today**: `--init` scaffolds directories; nothing is spec-aware.
- **Design**: `--import-openapi <spec-or-url> [--out <dir>]`. The argument is
  a local spec file, a direct URL to a spec, or a plain base URL — in the
  last case the spec is auto-discovered by probing well-known paths
  (`/openapi.json`, `/swagger.json`, `/v3/api-docs`,
  `/swagger/v1/swagger.json`, …). Discovery only works when the API
  *publishes* its spec; endpoints cannot be inferred from a bare URL
  otherwise. The parsed OpenAPI 3.x document (JSON or YAML) then generates
  suite skeletons: one suite per tag,
  URL as `{{BASE_URL}}<path>`, method/content-type from the operation,
  payload stubs built from schema `example`/`default` values, and
  `expected_status` from the first 2xx response. Auth left as a TODO step
  placeholder. Generated files are starting points, not runnable truth.
- **Work**: new `importer` crate (keeps the workspace separation; `cli`
  only wires the flag). Dep: `openapiv3`, behind an `openapi` cargo feature
  so the default build stays lean. Shallow `$ref` resolution only; anything
  unresolved becomes a `_todo` marker. **Size: L** — the biggest item; risk
  is spec diversity, mitigated by treating output as scaffolding.

### 2.6 Benchmark baselines

- **Today**: each benchmark writes its own `benchmarks/<label>/` dir with
  `data.json`; runs are never compared. Pool sampling is already seed-stable,
  which makes run-to-run comparison meaningful.
- **Design**: `--baseline save[=name]` persists a compact summary
  (`baselines/<suite>/<name>.json`: per-phase median/p90/p99, rps, error
  rate + metadata: date, env, pool size, seed). `--baseline check[=name]
  [--tolerance <pct>]` compares the current run and exits non-zero when p99
  (default metric, configurable) regresses beyond the tolerance. The HTML
  report overlays the baseline on the phase charts and prints a verdict.
- **Work**: `core` — baseline summary types + comparison math (unit-testable,
  pure); `cli` — flags + persistence; `reporter` — overlay + verdict.
  **Size: M.** CI-friendly by design (exit code).

### 2.7 History & trends

- **Today**: timestamped run dirs, no aggregation across runs.
- **Design**: append one summary line per run to
  `benchmarks/<suite>/history.jsonl` (same summary struct as 2.6 —
  build after it), and generate `trends.html`: median/p99, error rate and
  rps plotted over runs. Reuses the time-series SVG line-chart helper from
  the load-profile feature (§1, work item 5) — same self-contained offline
  HTML approach.
- **Work**: `reporter` — trend page; `cli` — history append + `--trends`
  flag. **Size: M**, mostly reuse. Depends on 2.6 and §1-item-5.

### 2.8 Tags & skip

- **Today**: the only filter is `-n/--names`; no test metadata.
- **Design**: `tags: ["smoke", "slow"]` and `skip: "<reason>"` on
  `TestRequest`. CLI `--tags smoke,-slow` (include/exclude, exclusion wins).
  Skipped and filtered-out tests are *reported* as skipped (console + HTML)
  rather than silently absent; steps always run (captures); the benchmark
  pool respects the same filter.
- **Work**: `core` — two fields; `cli` — flag + filter (next to the `--names`
  logic); `runner` — skip short-circuit in the sequencer; `reporter` —
  skipped state. **Size: S/M.**

## 3. Not retained

For the record, surveyed but not planned: partial/subset body match,
targeted per-pointer assertions, header & response-time assertions,
retries + configurable timeouts, `for_each` in compare mode, runtime-loaded
environments, `validate` mode + published schema, JUnit/JSON CI export.
