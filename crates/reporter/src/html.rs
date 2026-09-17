//! HTML reporter for `--reports`: each suite run produces
//! `reports/<suite>_<timestamp>/` containing `data.json` (raw results) and
//! `index.html`, a self-contained offline report focused on pass/fail and
//! the differences between expected and received responses.
//!
//! Secrets hygiene: request and response headers, payloads and bodies
//! are all serialized through a redaction pass that masks any value whose
//! key looks sensitive (secret, password, token, authorization, api_key).

use std::path::{Path, PathBuf};

use serde_json::Value;
use vantage_core::logger::TestLogger;
use vantage_core::result::{RequestType, TestResult};

pub struct HtmlReporter {
    root: PathBuf,
    suite: Option<String>,
    /// Set for grouped runs (`--group --reports`): one report aggregating all
    /// the group's suites, titled and named after the group.
    group: Option<String>,
    /// Primary environment name (`-e`); shown in the report header.
    environment: Option<String>,
    /// Compare environment name (`-c`); when set, the run is a compare run and
    /// the report labels each diff side with its environment.
    compare_environment: Option<String>,
    verbose: bool,
    results: Vec<TestResult>,
    /// Per-step timings (`--metrics`) as JSON, embedded in the report. `Null`
    /// when metrics were not collected.
    metrics: Value,
    written: bool,
    /// The directory written by the last successful `log_all`, surfaced via
    /// [`TestLogger::report_path`] so callers can e.g. email the report.
    report_dir: Option<std::path::PathBuf>,
}

impl HtmlReporter {
    #[must_use]
    pub fn new(root: PathBuf, verbose: bool) -> Self {
        Self {
            root,
            suite: None,
            group: None,
            environment: None,
            compare_environment: None,
            verbose,
            results: Vec::new(),
            metrics: Value::Null,
            written: false,
            report_dir: None,
        }
    }

    fn suite_stem(&self) -> String {
        self.suite
            .as_deref()
            .map(|s| {
                Path::new(s)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("suite")
                    .to_string()
            })
            .unwrap_or_else(|| "suite".to_string())
    }

    /// Per-suite roll-up (success rate, duration, timing) keyed by the `suite`
    /// label stamped on each result, in order of first appearance. Used only
    /// in grouped reports.
    fn suite_breakdown(&self, tests: &[&TestResult]) -> Vec<Value> {
        let mut order: Vec<String> = Vec::new();
        let mut durations: std::collections::HashMap<String, Vec<u128>> =
            std::collections::HashMap::new();
        let mut passed: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

        for r in tests {
            let name = r.suite.clone().unwrap_or_else(|| self.suite_stem());
            if !durations.contains_key(&name) {
                order.push(name.clone());
            }
            durations.entry(name.clone()).or_default().push(r.duration);
            let entry = passed.entry(name).or_default();
            if r.is_success {
                *entry += 1;
            }
        }

        order
            .into_iter()
            .map(|name| {
                let durs = durations.remove(&name).unwrap_or_default();
                let pass = passed.get(&name).copied().unwrap_or(0);
                suite_row(&name, durs, pass)
            })
            .collect()
    }

    /// `<stem>[_compare]_<timestamp>`: grouped runs are named after the
    /// group, single suites keep their stem; compare runs are marked so they
    /// are easy to tell apart from regular runs of the same suite.
    fn report_dir_name(&self) -> String {
        let stem = self
            .group
            .as_deref()
            .map(slug)
            .unwrap_or_else(|| self.suite_stem());
        let marker = if self.compare_environment.is_some() {
            "_compare"
        } else {
            ""
        };
        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        format!("{stem}{marker}_{timestamp}")
    }

    /// The full report payload: the run meta (counters, per-suite roll-up,
    /// metrics) plus every result, redacted of anything secret-looking.
    fn payload(&self) -> anyhow::Result<Value> {
        let tests: Vec<&TestResult> = self
            .results
            .iter()
            .filter(|r| r.request_type == RequestType::Test)
            .collect();
        let passed = tests.iter().filter(|r| r.is_success).count();
        let total_duration: u128 = tests.iter().map(|r| r.duration).sum();
        let suites = self.suite_breakdown(&tests);

        let mut results_json = serde_json::to_value(&self.results)?;
        redact_sensitive(&mut results_json);

        Ok(serde_json::json!({
            "meta": {
                "group": self.group.clone(),
                "suite": self.suite.clone().unwrap_or_default(),
                "environment": self.environment.clone(),
                "compare_environment": self.compare_environment.clone(),
                "generated_at": chrono::Local::now().to_rfc3339(),
                "total": tests.len(),
                "passed": passed,
                "failed": tests.len() - passed,
                "total_duration_ms": total_duration,
                "suites": suites,
                "metrics": self.metrics.clone(),
            },
            "results": results_json,
        }))
    }

    /// Writes `data.json` + `index.html`; returns the report directory.
    fn write_files(&self) -> anyhow::Result<PathBuf> {
        let dir = self.root.join(self.report_dir_name());
        std::fs::create_dir_all(&dir)?;

        let json = serde_json::to_string_pretty(&self.payload()?)?;
        std::fs::write(dir.join("data.json"), &json)?;

        let embedded = json.replace("</", "<\\/");
        let html = HTML_TEMPLATE
            .replace("__METRICS_JS__", crate::metrics_panel::METRICS_PANEL_JS)
            .replace("__DATA__", &embedded);
        std::fs::write(dir.join("index.html"), html)?;

        Ok(dir)
    }
}

/// One suite's roll-up row for a grouped report.
fn suite_row(name: &str, mut durations: Vec<u128>, passed: usize) -> Value {
    let total = durations.len();
    let total_duration: u128 = durations.iter().sum();
    durations.sort_unstable();
    let median = if durations.is_empty() {
        0
    } else {
        vantage_core::stats::calculate_median(&durations)
    };
    serde_json::json!({
        "name": name,
        "total": total,
        "passed": passed,
        "failed": total - passed,
        "total_duration_ms": total_duration,
        "median_ms": median,
    })
}

/// Filesystem-friendly slug for a group name (used as the report directory
/// prefix). Non-alphanumeric runs collapse to a single underscore.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.trim().chars() {
        if c.is_alphanumeric() {
            out.push(c);
            prev_sep = false;
        } else if !prev_sep {
            out.push('_');
            prev_sep = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "group".to_string()
    } else {
        trimmed
    }
}

/// Recursively redacts values whose key suggests a secret.
fn redact_sensitive(value: &mut Value) {
    const SENSITIVE: &[&str] = &[
        "secret",
        "password",
        "token",
        "authorization",
        "api_key",
        "cookie",
    ];

    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let lower = key.to_lowercase();
                if SENSITIVE.iter().any(|s| lower.contains(s)) {
                    *child = Value::String("***redacted***".to_string());
                } else {
                    redact_sensitive(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_sensitive(item);
            }
        }
        _ => {}
    }
}

impl TestLogger for HtmlReporter {
    fn set_suite(&mut self, name: &str) {
        self.suite = Some(name.to_string());
    }

    fn set_group(&mut self, name: &str) {
        self.group = Some(name.to_string());
    }

    fn set_environments(&mut self, environment: &str, compare_environment: Option<&str>) {
        self.environment = Some(environment.to_string());
        self.compare_environment = compare_environment.map(str::to_string);
    }

    fn set_metrics(&mut self, metrics: Value) {
        self.metrics = metrics;
    }

    fn enqueue_msg(&mut self, _msg: String) {}

    fn enqueue(&mut self, result: TestResult, _position: usize, _total: usize) {
        self.results.push(result);
    }

    fn log_all(&mut self) {
        if self.written {
            return;
        }
        match self.write_files() {
            Ok(dir) => {
                self.written = true;
                println!("Rapport : {}", dir.join("index.html").display());
                self.report_dir = Some(dir);
            }
            Err(e) => eprintln!("Failed to write HTML report: {e}"),
        }
    }

    fn report_path(&self) -> Option<std::path::PathBuf> {
        self.report_dir.clone()
    }

    fn is_verbose(&self) -> bool {
        self.verbose
    }

    fn summary(&mut self) {}
}

const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="fr">
<head>
<meta charset="utf-8">
<title>vantage report</title>
<style>
  :root { --bg:#101418; --card:#1a2027; --text:#e6e9ec; --dim:#8a94a0;
          --accent:#4da3ff; --ok:#3fb96f; --err:#e05555; --warn:#e0a14d; }
  body { background:var(--bg); color:var(--text); margin:0;
         font:14px/1.5 "Segoe UI",system-ui,sans-serif; }
  .wrap { max-width:1200px; margin:0 auto; padding:24px; }
  h1 { font-size:20px; margin:0 0 4px; }
  .sub { color:var(--dim); margin-bottom:24px; }
  .cards { display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr));
           gap:12px; margin-bottom:20px; }
  .card { background:var(--card); border-radius:8px; padding:14px 16px; }
  .card .v { font-size:24px; font-weight:600; }
  .card .l { color:var(--dim); font-size:12px; text-transform:uppercase; }
  .v.ok { color:var(--ok); } .v.err { color:var(--err); }
  .filters { margin-bottom:12px; }
  .filters button { background:var(--card); color:var(--text); border:1px solid #2a313a;
                    border-radius:6px; padding:6px 14px; margin-right:8px; cursor:pointer; }
  .filters button.active { border-color:var(--accent); color:var(--accent); }
  .row { background:var(--card); border-radius:8px; margin-bottom:6px; overflow:hidden; }
  .row-head { display:grid; grid-template-columns:64px 1fr 110px 90px 160px;
              gap:10px; padding:10px 14px; align-items:center; cursor:pointer; }
  .row.action .row-head { padding-left:40px; opacity:0.75; cursor:default; }
  .badge { font-size:11px; font-weight:700; text-align:center; border-radius:4px; padding:3px 0; }
  .badge.pass { background:#173c28; color:var(--ok); }
  .badge.fail { background:#43201f; color:var(--err); }
  .badge.warn { background:#42351c; color:var(--warn); }
  .status-cell, .dur-cell { text-align:right; color:var(--dim); }
  .status-cell b.bad { color:var(--err); }
  .durbar { background:#2a313a; border-radius:3px; height:8px; position:relative; }
  .durbar div { background:var(--accent); height:8px; border-radius:3px; }
  .row.failed .durbar div { background:var(--err); }
  .detail { display:none; border-top:1px solid #2a313a; padding:12px 16px; }
  .row.open .detail { display:block; }
  .errmsg { color:var(--err); white-space:pre-wrap; margin-bottom:12px;
            font-family:Consolas,monospace; font-size:12px; }
  .diff { display:grid; grid-template-columns:1fr 1fr; gap:10px; }
  .diff h3 { font-size:12px; color:var(--dim); margin:0 0 6px; }
  pre { background:#0c1014; border-radius:6px; padding:10px; overflow:auto;
        font:12px/1.45 Consolas,monospace; margin:0; max-height:420px; }
  pre .hl { background:#3a1d1d; display:inline-block; width:100%; }
  .payload pre { max-height:200px; }
  .payload h3 { font-size:12px; color:var(--dim); margin:12px 0 6px; }
  .suite-sec { display:flex; align-items:baseline; justify-content:space-between;
               gap:12px; margin:22px 0 8px; padding-bottom:6px;
               border-bottom:1px solid #2a313a; }
  .suite-sec .suite-name { font-size:15px; font-weight:600; }
  .suite-sec .suite-stats { display:flex; gap:14px; color:var(--dim); font-size:12px; }
  .suite-sec .suite-stats b { font-weight:700; }
  .suite-sec .suite-stats .ok { color:var(--ok); }
  .suite-sec .suite-stats .err { color:var(--err); }
  .metrics-sec { background:var(--card); border-radius:8px; padding:16px; margin:0 0 20px; }
  .metrics-sec h2 { font-size:14px; margin:0 0 12px; color:var(--dim); }
  .metrics-sec svg text { fill:var(--dim); font-size:11px; }
  .metrics-sec table { width:100%; border-collapse:collapse; margin-top:12px; }
  .metrics-sec th, .metrics-sec td { text-align:right; padding:6px 10px; border-bottom:1px solid #2a313a; }
  .metrics-sec th:first-child, .metrics-sec td:first-child { text-align:left; }
  .metrics-sec th { color:var(--dim); font-weight:500; }
  .metrics-sec tr.over td { background:#42351c; }
  .metrics-sec .note { color:var(--dim); font-size:12px; margin-top:8px; }
  .info { cursor:help; color:var(--accent); font-weight:700; position:relative; }
  .info:hover::after, .info:focus::after { content:attr(data-tip); position:absolute; left:0; top:150%;
    z-index:20; background:#0c1014; border:1px solid #2a313a; border-radius:6px; padding:8px 10px;
    width:270px; color:var(--text); font-weight:400; font-size:12px; line-height:1.5;
    white-space:normal; box-shadow:0 4px 16px rgba(0,0,0,0.4); }
</style>
</head>
<body>
<div class="wrap">
  <h1 id="title"></h1>
  <div class="sub" id="subtitle"></div>
  <div class="cards" id="cards"></div>
  <div id="metrics"></div>
  <div class="filters" id="filters">
    <button data-f="all" class="active">Tous</button>
    <button data-f="failed">Echecs</button>
    <button data-f="passed">Succes</button>
  </div>
  <div id="rows"></div>
</div>
<script>
const DATA = __DATA__;
const meta = DATA.meta, results = DATA.results;
const isGroup = !!meta.group;
const isCompare = !!meta.compare_environment;

document.getElementById("title").textContent =
  "Rapport - " + (isGroup ? meta.group : (meta.suite || "suite"));

let envTxt = "";
if (isCompare) {
  envTxt = "Compare : " + meta.environment + " (gauche) vs " + meta.compare_environment + " (droite)";
} else if (meta.environment) {
  envTxt = "Environnement : " + meta.environment;
}
document.getElementById("subtitle").textContent =
  meta.generated_at + (envTxt ? "  |  " + envTxt : "");

const esc = s => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;");
const pretty = v => v == null ? "(absent)" : JSON.stringify(v, null, 2);
const rateClass = r => r === 100 ? 'ok' : (r < 80 ? 'err' : '');
const card = ([l, v, c]) =>
  '<div class="card"><div class="v ' + c + '">' + v + '</div><div class="l">' + l + '</div></div>';

// Side-by-side diff: highlight lines missing from the other side. Labels
// default to Attendu/Recu but compare runs pass the environment names.
function diffBlock(leftVal, rightVal, leftLabel, rightLabel) {
  const e = pretty(leftVal).split("\n"), r = pretty(rightVal).split("\n");
  const eSet = new Set(e), rSet = new Set(r);
  const mark = (lines, other) => lines.map(l =>
    other.has(l) ? esc(l) : '<span class="hl">' + esc(l) + '</span>').join("\n");
  return '<div class="diff">' +
    '<div><h3>' + esc(leftLabel) + '</h3><pre>' + mark(e, rSet) + '</pre></div>' +
    '<div><h3>' + esc(rightLabel) + '</h3><pre>' + mark(r, eSet) + '</pre></div></div>';
}

// Compare runs label each side with its environment (primary on the left,
// compare on the right). Regular runs keep Attendu (expected) vs Recu.
function diffFor(r) {
  return isCompare
    ? diffBlock(r.body, r.expected_body, meta.environment, meta.compare_environment)
    : diffBlock(r.expected_body, r.body, "Attendu", "Recu");
}

const maxDur = Math.max(...results.map(r => r.duration), 1);
function rowHtml(r) {
  const isAction = r.request_type === "Action";
  const badge = r.is_success ? (isAction ? ["warn", "OK"] : ["pass", "PASS"])
                             : (isAction ? ["warn", "WARN"] : ["fail", "FAIL"]);
  const statusTxt = isAction ? "" :
    (r.status ?? "-") + " / " + r.expected_status +
    ((r.status ?? -1) !== r.expected_status ? " !" : "");
  const cls = "row" + (isAction ? " action" : "") + (r.is_success ? "" : " failed");
  let detail = "";
  if (!isAction) {
    detail = '<div class="detail">' +
      (r.error ? '<div class="errmsg">' + esc(r.error) + '</div>' : '') +
      diffFor(r) +
      (Object.keys(r.request_headers || {}).length ?
        '<div class="payload"><h3>Request headers</h3><pre>' + esc(pretty(r.request_headers)) + '</pre></div>' : '') +
      (r.payload ? '<div class="payload"><h3>Request body</h3><pre>' + esc(pretty(r.payload)) + '</pre></div>' : '') +
      (Object.keys(r.headers || {}).length ?
        '<div class="payload"><h3>Response headers</h3><pre>' + esc(pretty(r.headers)) + '</pre></div>' : '') +
      '</div>';
  }
  return '<div class="' + cls + '" data-ok="' + r.is_success + '" data-action="' + isAction + '">' +
    '<div class="row-head" onclick="this.parentElement.classList.toggle(\'open\')">' +
    '<div class="badge ' + badge[0] + '">' + badge[1] + '</div>' +
    '<div>' + esc(r.name) + '</div>' +
    '<div class="status-cell">' + (statusTxt.includes("!") ? "<b class=\"bad\">" + statusTxt + "</b>" : statusTxt) + '</div>' +
    '<div class="dur-cell">' + r.duration + ' ms</div>' +
    '<div class="durbar"><div style="width:' + Math.max(2, 100 * r.duration / maxDur) + '%"></div></div>' +
    '</div>' + detail + '</div>';
}

const rate = meta.total ? Math.round(100 * meta.passed / meta.total) : 0;

if (isGroup) {
  // Global summary: success rate only, no group-level timing.
  document.getElementById("cards").innerHTML = [
    ['Suites', meta.suites.length, ''],
    ['Tests', meta.total, ''],
    ['Succes', meta.passed, 'ok'],
    ['Echecs', meta.failed, meta.failed ? 'err' : 'ok'],
    ['Taux de reussite', rate + '%', rateClass(rate)],
  ].map(card).join("");

  // One section per suite (success rate, duration, timing), then its rows.
  document.getElementById("rows").innerHTML = meta.suites.map(s => {
    const sr = s.total ? Math.round(100 * s.passed / s.total) : 0;
    const head = '<div class="suite-sec">' +
      '<div class="suite-name">' + esc(s.name) + '</div>' +
      '<div class="suite-stats">' +
        '<span><b class="' + rateClass(sr) + '">' + sr + '%</b> reussite</span>' +
        '<span>' + s.passed + '/' + s.total + '</span>' +
        '<span>' + s.total_duration_ms + ' ms</span>' +
        '<span>mediane ' + s.median_ms + ' ms</span>' +
      '</div></div>';
    const rows = results
      .filter(r => (r.suite || meta.suite) === s.name)
      .map(rowHtml).join("");
    return head + rows;
  }).join("");
} else {
  const durs = results.filter(r => r.request_type === "Test").map(r => r.duration).sort((a, b) => a - b);
  const med = durs.length ? durs[Math.floor(durs.length / 2)] : 0;
  document.getElementById("cards").innerHTML = [
    ['Tests', meta.total, ''],
    ['Succes', meta.passed, 'ok'],
    ['Echecs', meta.failed, meta.failed ? 'err' : 'ok'],
    ['Taux de reussite', rate + '%', rateClass(rate)],
    ['Duree totale', meta.total_duration_ms + ' ms', ''],
    ['Mediane', med + ' ms', ''],
  ].map(card).join("");
  document.getElementById("rows").innerHTML = results.map(rowHtml).join("");
}

document.getElementById("filters").addEventListener("click", e => {
  const f = e.target.dataset.f;
  if (!f) return;
  document.querySelectorAll(".filters button").forEach(b => b.classList.toggle("active", b === e.target));
  document.querySelectorAll(".row").forEach(row => {
    const ok = row.dataset.ok === "true", action = row.dataset.action === "true";
    row.style.display =
      f === "all" ? "" :
      f === "failed" ? ((!ok && !action) ? "" : "none") :
      ((ok && !action) ? "" : "none");
  });
});

// ----- per-step metrics (--metrics) -----
renderMetrics(meta.metrics, document.getElementById("metrics"));

__METRICS_JS__
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(name: &str, success: bool) -> TestResult {
        let mut r = TestResult::new(RequestType::Test)
            .with_name(name.to_string())
            .with_status(if success { 200 } else { 500 })
            .with_duration(42)
            .with_success(success)
            .with_expected_body(json!({"unitPrice": 1.5}))
            .with_body(json!({"unitPrice": if success { 1.5 } else { 9.9 }}));
        r.payload = Some(json!({"_sku": "A", "client_secret": "TOP-SECRET"}));
        r.request_headers
            .insert("Authorization".to_string(), "Bearer xyz".to_string());
        r.request_headers
            .insert("X-Correlation-Id".to_string(), "abc-123".to_string());
        r.headers
            .insert("set-cookie".to_string(), "session_token=zzz".to_string());
        r
    }

    fn reporter_in_temp(tag: &str) -> (HtmlReporter, PathBuf) {
        let root = std::env::temp_dir().join(format!("vantage_html_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (HtmlReporter::new(root.clone(), false), root)
    }

    fn report_dir(root: &Path) -> PathBuf {
        std::fs::read_dir(root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    #[test]
    fn metrics_are_embedded_and_rendered_when_set() {
        let (mut reporter, root) = reporter_in_temp("metrics");
        reporter.set_metrics(json!([{
            "name": "http", "count": 3, "total_ms": 30.0, "median_ms": 10.0,
            "p90_ms": 12.0, "expected_ms": 250.0, "description": "Network round-trip"
        }]));
        reporter.enqueue(result("t", true), 1, 1);
        reporter.log_all();

        let dir = report_dir(&root);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert_eq!(parsed["meta"]["metrics"][0]["name"], "http");

        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(
            html.contains("Performance par etape") && html.contains("renderMetrics"),
            "the metrics section must be present in the report"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_metrics_section_data_when_unset() {
        let (mut reporter, root) = reporter_in_temp("nometrics");
        reporter.enqueue(result("t", true), 1, 1);
        reporter.log_all();
        let dir = report_dir(&root);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert!(
            parsed["meta"]["metrics"].is_null(),
            "metrics null when not collected"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_data_json_and_html_with_summary() {
        let (mut reporter, root) = reporter_in_temp("basic");
        reporter.set_suite("test-suite/mySuite.json");
        reporter.enqueue(result("ok test", true), 1, 2);
        reporter.enqueue(result("ko test", false), 2, 2);
        reporter.log_all();

        let dir = report_dir(&root);
        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mySuite_")
        );

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert_eq!(data["meta"]["total"], 2);
        assert_eq!(data["meta"]["passed"], 1);
        assert_eq!(data["meta"]["failed"], 1);
        assert_eq!(data["results"][1]["body"]["unitPrice"], 9.9);

        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(!html.contains("__DATA__"));
        assert!(html.contains("ko test"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn secrets_are_never_persisted() {
        let (mut reporter, root) = reporter_in_temp("secrets");
        reporter.set_suite("s.json");
        reporter.enqueue(result("t", true), 1, 1);
        reporter.log_all();

        let raw = std::fs::read_to_string(report_dir(&root).join("data.json")).unwrap();
        assert!(
            !raw.contains("TOP-SECRET"),
            "payload secrets must be redacted"
        );
        assert!(
            !raw.contains("Bearer xyz"),
            "header secrets must be redacted"
        );
        assert!(raw.contains("***redacted***"));

        let data: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            data["results"][0]["request_headers"]["Authorization"], "***redacted***",
            "sent headers must be present, redacted"
        );
        assert_eq!(
            data["results"][0]["request_headers"]["X-Correlation-Id"], "abc-123",
            "non-sensitive headers must stay readable"
        );
        assert_eq!(
            data["results"][0]["headers"]["set-cookie"], "***redacted***",
            "cookies carry session tokens"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compare_run_marks_directory_and_records_environments() {
        let (mut reporter, root) = reporter_in_temp("compare");
        reporter.set_suite("test-suite/mySuite.json");
        reporter.set_environments("test", Some("staging"));
        reporter.enqueue(result("t", false), 1, 1);
        reporter.log_all();

        let dir = report_dir(&root);
        let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            dir_name.starts_with("mySuite_compare_"),
            "compare runs must be marked in the directory name: {dir_name}"
        );

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert_eq!(data["meta"]["environment"], "test");
        assert_eq!(data["meta"]["compare_environment"], "staging");

        // The environment names reach the report so the diff panels and header
        // can label which environment produced which side.
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(html.contains("staging"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_compare_run_directory_has_no_compare_marker() {
        let (mut reporter, root) = reporter_in_temp("plain");
        reporter.set_suite("test-suite/mySuite.json");
        reporter.set_environments("test", None);
        reporter.enqueue(result("t", true), 1, 1);
        reporter.log_all();

        let dir_name = report_dir(&root)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(dir_name.starts_with("mySuite_"), "dir: {dir_name}");
        assert!(!dir_name.contains("_compare_"), "dir: {dir_name}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn log_all_writes_only_once() {
        let (mut reporter, root) = reporter_in_temp("once");
        reporter.set_suite("s.json");
        reporter.enqueue(result("t", true), 1, 1);
        reporter.log_all();
        reporter.log_all();

        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slug_makes_group_names_filesystem_safe() {
        assert_eq!(slug("Live services"), "Live_services");
        assert_eq!(slug("  a / b  "), "a_b");
        assert_eq!(slug("Prix & Références"), "Prix_Références");
        assert_eq!(slug("***"), "group");
        assert_eq!(slug("clean"), "clean");
    }

    #[test]
    fn grouped_report_orders_suites_by_first_appearance_and_computes_median() {
        let (mut reporter, root) = reporter_in_temp("group_order");
        reporter.set_group("G");

        // Interleave two suites; suiteB appears first.
        let durations = [
            ("suiteB", 10u128),
            ("suiteA", 50),
            ("suiteB", 30),
            ("suiteA", 10),
        ];
        for (i, (name, dur)) in durations.iter().enumerate() {
            let mut r = result(&format!("t{i}"), true).with_duration(*dur);
            r.suite = Some((*name).to_string());
            reporter.enqueue(r, i + 1, durations.len());
        }
        reporter.log_all();

        let data: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(report_dir(&root).join("data.json")).unwrap(),
        )
        .unwrap();
        let suites = data["meta"]["suites"].as_array().unwrap();
        assert_eq!(suites[0]["name"], "suiteB", "first-seen suite comes first");
        assert_eq!(suites[1]["name"], "suiteA");
        // suiteB durations [10,30] -> median 20; suiteA [50,10] sorted [10,50] -> 30.
        assert_eq!(suites[0]["median_ms"], 20);
        assert_eq!(suites[0]["total_duration_ms"], 40);
        assert_eq!(suites[1]["median_ms"], 30);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn single_suite_report_has_no_group_meta() {
        let (mut reporter, root) = reporter_in_temp("single_meta");
        reporter.set_suite("s.json");
        reporter.enqueue(result("t", true), 1, 1);
        reporter.log_all();

        let data: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(report_dir(&root).join("data.json")).unwrap(),
        )
        .unwrap();
        assert!(data["meta"]["group"].is_null(), "no group for single runs");
        // Results carry no suite tag outside group mode.
        assert!(data["results"][0].get("suite").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grouped_report_is_named_after_group_and_breaks_down_by_suite() {
        let (mut reporter, root) = reporter_in_temp("group");
        reporter.set_group("Live services");
        reporter.set_suite("ignored-in-group-mode.json");

        let mut a = result("a1", true);
        a.suite = Some("suiteA".to_string());
        let mut b = result("a2", false);
        b.suite = Some("suiteA".to_string());
        let mut c = result("b1", true);
        c.suite = Some("suiteB".to_string());
        reporter.enqueue(a, 1, 3);
        reporter.enqueue(b, 2, 3);
        reporter.enqueue(c, 3, 3);
        reporter.log_all();

        let dir = report_dir(&root);
        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Live_services_"),
            "report dir should be named after the group"
        );

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert_eq!(data["meta"]["group"], "Live services");
        assert_eq!(data["meta"]["total"], 3);
        assert_eq!(data["meta"]["passed"], 2);

        let suites = data["meta"]["suites"].as_array().unwrap();
        assert_eq!(suites.len(), 2);
        assert_eq!(suites[0]["name"], "suiteA");
        assert_eq!(suites[0]["total"], 2);
        assert_eq!(suites[0]["passed"], 1);
        assert_eq!(suites[0]["failed"], 1);
        assert_eq!(suites[1]["name"], "suiteB");
        assert_eq!(suites[1]["total"], 1);
        assert_eq!(suites[1]["passed"], 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn actions_are_kept_out_of_test_totals() {
        let (mut reporter, root) = reporter_in_temp("actions");
        reporter.set_suite("s.json");
        reporter.enqueue(result("t", true), 1, 1);
        reporter.enqueue(
            TestResult::new(RequestType::Action)
                .with_name("after: log".to_string())
                .with_success(true),
            1,
            1,
        );
        reporter.log_all();

        let data: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(report_dir(&root).join("data.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(data["meta"]["total"], 1, "actions are not tests");
        assert_eq!(data["results"].as_array().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }
}
