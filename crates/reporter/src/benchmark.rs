//! Formatting and persistence of `--benchmark` reports.
//!
//! Each run produces `benchmarks/<suite>_<timestamp>/` containing:
//! - `data.json`: the raw samples, for later aggregation;
//! - `index.html`: a self-contained, offline report (inline SVG charts,
//!   no CDN) in the spirit of sitespeed.io.

use std::path::{Path, PathBuf};

use anyhow::Context;
use vantage_core::benchmark::{BenchReport, LoadReport, PhaseReport};

fn phase_label(concurrency: usize) -> String {
    if concurrency <= 1 {
        "sequentiel".to_string()
    } else {
        format!("parallele x{concurrency}")
    }
}

/// Median of a sorted slice; 0 when empty (empty phases stay displayable).
fn median(sorted: &[u128]) -> u128 {
    if sorted.is_empty() {
        0
    } else {
        vantage_core::stats::calculate_median(sorted)
    }
}

/// P90 of a sorted slice; 0 when empty.
fn p90(sorted: &[u128]) -> u128 {
    if sorted.is_empty() {
        0
    } else {
        vantage_core::stats::calculate_p90(sorted)
    }
}

fn phase_row(phase: &PhaseReport) -> String {
    let sorted = phase.sorted_durations();
    format!(
        "{:<14} {:>4} {:>5} {:>9} {:>9} {:>8} {:>8} {:>7.1}",
        phase_label(phase.concurrency),
        phase.samples.len(),
        phase.error_count(),
        format!("{}ms", phase.wall_ms),
        format!("{}ms", median(&sorted)),
        format!("{}ms", p90(&sorted)),
        format!("{}ms", sorted.last().copied().unwrap_or(0)),
        phase.requests_per_second(),
    )
}

/// The network-latency line. An empty probe side means the probe could not
/// measure (dead port, firewall): show n/a, never a too-good-to-be-true
/// 0.0ms.
fn latency_line(latency: &vantage_core::benchmark::LatencyReport) -> String {
    let side = |samples: &[u128], median_ms: f64| {
        if samples.is_empty() {
            "n/a".to_string()
        } else {
            format!("~{median_ms:.1}ms")
        }
    };
    format!(
        "Latence reseau : TCP {} | HTTP {} ({} sondes)\n",
        side(&latency.tcp_us, latency.tcp_median_ms()),
        side(&latency.http_us, latency.http_median_ms()),
        latency.tcp_us.len().max(latency.http_us.len())
    )
}

/// The success/error split over every measured request of every phase.
fn split_line(phases: &[PhaseReport]) -> String {
    let mut ok: Vec<u128> = vec![];
    let mut ko: Vec<u128> = vec![];
    for phase in phases {
        for sample in &phase.samples {
            if sample.is_success {
                ok.push(sample.duration_ms);
            } else {
                ko.push(sample.duration_ms);
            }
        }
    }
    ok.sort_unstable();
    ko.sort_unstable();

    format!(
        "\nSucces : {} req, mediane {}ms | Erreurs : {} req, mediane {}ms\n",
        ok.len(),
        median(&ok),
        ko.len(),
        median(&ko)
    )
}

/// Builds the human-readable benchmark report.
#[must_use]
pub fn format_report(file: &str, report: &BenchReport) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "\nBENCHMARK - {file} (pool : {} requetes, warm-up : {})\n",
        report.pool_size, report.warmup_count
    ));

    if let Some(latency) = &report.latency {
        out.push_str(&latency_line(latency));
    }
    out.push('\n');
    out.push_str(&format!(
        "{:<14} {:>4} {:>5} {:>9} {:>9} {:>8} {:>8} {:>7}\n",
        "Phase", "Req", "Err", "Total", "Mediane", "P90*", "Max", "Req/s"
    ));

    for phase in &report.phases {
        out.push_str(&phase_row(phase));
        out.push('\n');
    }

    if let Some(reason) = &report.stop_reason {
        out.push_str(&format!("\nArret : {reason}\n"));
    }

    out.push_str(&split_line(&report.phases));
    out.push_str("(*) P90 et Max indicatifs a cette taille d'echantillon\n");

    if let Some(load) = &report.load {
        out.push_str(&load_summary(load));
    }

    out
}

/// The console summary of the load-profile portion of a run: a per-stage
/// roll-up (dispatched vs completed requests and errors, from the per-second
/// buckets attributed to each stage's time window), the run totals, and the
/// adaptive-stop reason.
fn load_summary(load: &LoadReport) -> String {
    let mut out = format!("\nProfil de charge : {} etape(s)\n", load.stages.len());
    out.push_str(&format!(
        "{:<3} {:<6} {:>6} {:>6} {:>7} {:>6} {:>5}\n",
        "#", "Forme", "Duree", "Cible", "Envoi", "Fini", "Err"
    ));

    // Each stage owns the buckets whose second falls in its wall-clock window;
    // completions that spill past the profile's end are not attributed to a
    // stage but still count toward the run totals below.
    let mut stage_start = 0u64;
    for (index, stage) in load.stages.iter().enumerate() {
        let duration_s = stage.duration.as_secs();
        let stage_end = stage_start + duration_s;
        let in_stage = load
            .buckets
            .iter()
            .filter(|b| b.second >= stage_start && b.second < stage_end);
        let (mut sent, mut completed, mut errors) = (0usize, 0usize, 0usize);
        for bucket in in_stage {
            sent += bucket.sent;
            completed += bucket.completed;
            errors += bucket.errors;
        }
        let target = stage
            .target_cps
            .map_or_else(|| "-".to_string(), |cps| format!("{cps:.0}"));
        out.push_str(&format!(
            "{:<3} {:<6} {:>5}s {:>6} {:>7} {:>6} {:>5}\n",
            index + 1,
            stage.shape.as_str(),
            duration_s,
            target,
            sent,
            completed,
            errors
        ));
        stage_start = stage_end;
    }

    let sent: usize = load.buckets.iter().map(|b| b.sent).sum();
    let completed: usize = load.buckets.iter().map(|b| b.completed).sum();
    let errors: usize = load.buckets.iter().map(|b| b.errors).sum();
    out.push_str(&format!(
        "Total : {sent} envoyees, {completed} terminees, {errors} erreurs\n"
    ));
    if let Some(reason) = &load.stop_reason {
        out.push_str(&format!("Arret (charge) : {reason}\n"));
    }
    out
}

/// Writes `data.json` and `index.html` under
/// `<root>/<suite-stem>_<timestamp>/` and returns the report directory.
pub fn write_report_files(
    suite_file: &str,
    environment: &str,
    report: &BenchReport,
    metrics: serde_json::Value,
    root: &Path,
) -> anyhow::Result<PathBuf> {
    let stem = Path::new(suite_file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("suite");
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let dir = root.join(format!("{stem}_{timestamp}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let payload = serde_json::json!({
        "meta": {
            "suite": suite_file,
            "environment": environment,
            "generated_at": chrono::Local::now().to_rfc3339(),
            "pool_size": report.pool_size,
            "warmup_count": report.warmup_count,
            "metrics": metrics,
        },
        "report": report,
    });
    let json = serde_json::to_string_pretty(&payload)?;
    std::fs::write(dir.join("data.json"), &json)?;

    // Escape every `<` so embedded JSON (test names, Server-Timing metric
    // names...) can never break out of the inline <script>, whether via
    // `</script>` or `<!--`.
    let embedded = json.replace('<', "\\u003c");
    let html = HTML_TEMPLATE
        .replace("__METRICS_JS__", crate::metrics_panel::METRICS_PANEL_JS)
        .replace("__DATA__", &embedded);
    std::fs::write(dir.join("index.html"), html)?;

    Ok(dir)
}

const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="fr">
<head>
<meta charset="utf-8">
<title>vantage benchmark</title>
<style>
  :root { --bg:#101418; --card:#1a2027; --text:#e6e9ec; --dim:#8a94a0;
          --accent:#4da3ff; --ok:#3fb96f; --err:#e05555; --warn:#e0a14d; }
  body { background:var(--bg); color:var(--text); margin:0;
         font:14px/1.5 "Segoe UI",system-ui,sans-serif; }
  .wrap { max-width:1100px; margin:0 auto; padding:24px; }
  h1 { font-size:20px; margin:0 0 4px; }
  .sub { color:var(--dim); margin-bottom:24px; }
  .cards { display:grid; grid-template-columns:repeat(auto-fit,minmax(170px,1fr));
           gap:12px; margin-bottom:24px; }
  .card { background:var(--card); border-radius:8px; padding:14px 16px; }
  .card .v { font-size:24px; font-weight:600; }
  .card .l { color:var(--dim); font-size:12px; text-transform:uppercase; }
  .card.stop .v { font-size:15px; color:var(--warn); }
  .panes { display:grid; grid-template-columns:1fr 1fr; gap:12px; margin-bottom:24px; }
  .pane { background:var(--card); border-radius:8px; padding:16px; }
  .pane h2 { font-size:14px; margin:0 0 12px; color:var(--dim); }
  .pane.full { grid-column:1 / -1; }
  table { width:100%; border-collapse:collapse; }
  th, td { text-align:right; padding:6px 10px; border-bottom:1px solid #2a313a; }
  th:first-child, td:first-child { text-align:left; }
  th { color:var(--dim); font-weight:500; }
  .note { color:var(--dim); font-size:12px; margin-top:8px; }
  svg text { fill:var(--dim); font-size:11px; }
  .metrics-sec { background:var(--card); border-radius:8px; padding:16px; margin-bottom:24px; }
  .metrics-sec h2 { font-size:14px; margin:0 0 12px; color:var(--dim); }
  .metrics-sec table { width:100%; border-collapse:collapse; margin-top:12px; }
  .metrics-sec tr.over td { background:#42351c; }
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
  <div class="panes">
    <div class="pane"><h2>Latence par niveau de concurrence (ms)</h2><div id="latency"></div></div>
    <div class="pane"><h2>Debit (req/s)</h2><div id="throughput"></div></div>
    <div class="pane full"><h2>Durees individuelles par phase</h2><div id="strip"></div>
      <div class="note">Chaque point est une requete. Rouge = erreur.</div></div>
    <div class="pane full"><h2>Durees par appel (ms)</h2><div id="percall"></div>
      <div class="note" id="percall-note"></div></div>
    <div class="pane full"><h2>Detail des phases</h2><div id="phases"></div>
      <div class="note">P90 et Max indicatifs a cette taille d'echantillon.</div></div>
  </div>
  <div id="load-section" style="display:none">
    <div class="panes">
      <div class="pane full"><h2>Debit dans le temps (req/s) - cible vs atteint</h2>
        <div id="load-throughput"></div>
        <div class="note">Gris = cible, vert = atteint (terminees/s). L'ecart revele la saturation.</div></div>
      <div class="pane full"><h2>Temps de reponse dans le temps (ms)</h2>
        <div id="load-latency"></div>
        <div class="note">Ligne = mediane, bande = P90-P99, points rouges = erreurs.</div></div>
      <div class="pane full"><h2>Profil de charge - etapes</h2><div id="load-stages"></div></div>
    </div>
  </div>
</div>
<script>
const DATA = __DATA__;

const phases = DATA.report.phases;
const esc = s => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;");
const label = c => c <= 1 ? "sequentiel" : ("x" + c);
const durations = p => p.samples.map(s => s.duration_ms).sort((a, b) => a - b);
const median = a => a.length ? (a.length % 2 ? a[(a.length - 1) / 2]
  : (a[a.length / 2 - 1] + a[a.length / 2]) / 2) : 0;
const p90 = a => a.length ? a[Math.floor((a.length - 1) * 0.9)] : 0;
const reqps = p => p.wall_ms ? (p.samples.length * 1000 / p.wall_ms) : 0;
const errors = p => p.samples.filter(s => !s.is_success).length;

document.getElementById("title").textContent = "Benchmark - " + DATA.meta.suite;
document.getElementById("subtitle").textContent =
  DATA.meta.environment + " - " + DATA.meta.generated_at +
  " - pool " + DATA.meta.pool_size + " req, warm-up " + DATA.meta.warmup_count;

// ----- cards -----
const all = phases.flatMap(p => p.samples);
const errCount = all.filter(s => !s.is_success).length;
const seq = phases.find(p => p.concurrency <= 1);
const bestRps = Math.max(...phases.map(reqps));
const cards = [
  ["Requetes mesurees", all.length],
  ["Erreurs", errCount + " (" + (all.length ? Math.round(100 * errCount / all.length) : 0) + "%)"],
  ["Mediane sequentielle", seq ? median(durations(seq)) + " ms" : "-"],
  ["Meilleur debit", bestRps.toFixed(1) + " req/s"],
];
if (DATA.report.latency) {
  const medMs = a => {
    const s = [...a].sort((x, y) => x - y);
    if (!s.length) return "0.0";
    const v = s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
    return (v / 1000).toFixed(1);
  };
  const sideMs = a => a.length ? medMs(a) + " ms" : "n/a";
  cards.push(["Latence TCP (mediane)", sideMs(DATA.report.latency.tcp_us)]);
  cards.push(["Latence HTTP (mediane)", sideMs(DATA.report.latency.http_us)]);
}
let cardsHtml = cards.map(([l, v]) =>
  '<div class="card"><div class="v">' + v + '</div><div class="l">' + l + '</div></div>').join("");
if (DATA.report.stop_reason) {
  cardsHtml += '<div class="card stop"><div class="v">' + DATA.report.stop_reason +
    '</div><div class="l">Arret</div></div>';
}
document.getElementById("cards").innerHTML = cardsHtml;

// ----- grouped bar chart: median + p90 -----
function barChart(items, w, h) {
  const max = Math.max(...items.flatMap(i => i.values), 1);
  const pad = 28, bw = (w - pad) / items.length;
  let s = '<svg viewBox="0 0 ' + w + ' ' + h + '" width="100%">';
  items.forEach((it, i) => {
    const x0 = pad + i * bw;
    const inner = bw * 0.7 / it.values.length;
    it.values.forEach((v, j) => {
      const bh = (h - 30) * v / max;
      s += '<rect x="' + (x0 + j * inner) + '" y="' + (h - 20 - bh) + '" width="' + (inner - 3) +
           '" height="' + bh + '" fill="' + it.colors[j] + '" rx="2"><title>' + v + ' ms</title></rect>';
    });
    s += '<text x="' + (x0 + bw * 0.35) + '" y="' + (h - 6) + '" text-anchor="middle">' + it.label + '</text>';
  });
  s += '<text x="2" y="12">' + Math.round(max) + '</text></svg>';
  return s;
}
document.getElementById("latency").innerHTML = barChart(
  phases.map(p => {
    const d = durations(p);
    return { label: label(p.concurrency), values: [median(d), p90(d)],
             colors: ["#4da3ff", "#2a5d94"] };
  }), 480, 200);
document.getElementById("throughput").innerHTML = barChart(
  phases.map(p => ({ label: label(p.concurrency),
                     values: [Number(reqps(p).toFixed(1))], colors: ["#3fb96f"] })), 480, 200);

// ----- strip plot -----
(function () {
  const w = 1000, h = 220, pad = 40;
  const maxD = Math.max(...all.map(s => s.duration_ms), 1);
  const bw = (w - pad) / phases.length;
  let s = '<svg viewBox="0 0 ' + w + ' ' + h + '" width="100%">';
  phases.forEach((p, i) => {
    const x0 = pad + i * bw + bw / 2;
    p.samples.forEach((smp, j) => {
      const jit = ((j * 37) % 21 - 10) * (bw / 60);
      const y = h - 24 - (h - 44) * smp.duration_ms / maxD;
      s += '<circle cx="' + (x0 + jit) + '" cy="' + y + '" r="4" fill="' +
           (smp.is_success ? "#4da3ff" : "#e05555") +
           '" fill-opacity="0.8" data-p="' + i + '" data-i="' + j + '"></circle>';
    });
    s += '<text x="' + x0 + '" y="' + (h - 6) + '" text-anchor="middle">' + label(p.concurrency) + '</text>';
  });
  s += '<text x="2" y="14">' + maxD + ' ms</text></svg>';
  document.getElementById("strip").innerHTML = s;
})();

// ----- per-call bars: latency (Server-Timing) vs processing -----
(function () {
  const hasTiming = all.some(s => s.server_ms != null);
  const partPalette = ["#4da3ff", "#3fb96f", "#e0a14d", "#b48ce0", "#4dc3d0", "#e08cb8"];
  const partColors = new Map();
  const partColor = name => {
    if (!partColors.has(name)) partColors.set(name, partPalette[partColors.size % partPalette.length]);
    return partColors.get(name);
  };
  const OTHER = "#2a5d94", LATENCY = "#8a94a0", ERROR = "#e05555";

  let html = "";
  phases.forEach(p => {
    const w = 1000, h = 170, pad = 34;
    const maxD = Math.max(...p.samples.map(s => s.duration_ms), 1);
    const step = (w - pad) / p.samples.length;
    const bw = Math.max(2, Math.min(20, step - 2));
    let s = '<div class="note" style="margin:10px 0 2px">' + label(p.concurrency) + '</div>' +
      '<svg viewBox="0 0 ' + w + ' ' + h + '" width="100%">';
    p.samples.forEach((smp, i) => {
      const x = pad + i * step;
      const bh = Math.max(1, (h - 36) * smp.duration_ms / maxD);
      const y0 = h - 16;
      const parts = smp.server_parts || [];
      const phaseIndex = phases.indexOf(p);
      const rect = (y, height, fill) =>
        '<rect x="' + x + '" y="' + y + '" width="' + bw + '" height="' + height +
        '" fill="' + fill + '" rx="1" data-p="' + phaseIndex + '" data-i="' + i + '"></rect>';

      if (smp.server_ms != null && smp.duration_ms > 0 && smp.is_success) {
        const latMs = Math.max(0, smp.duration_ms - smp.server_ms);
        const latH = bh * Math.min(1, latMs / smp.duration_ms);
        s += rect(y0 - latH, latH, LATENCY);
        const procH = bh - latH;
        let used = 0;
        parts.forEach(pt => {
          const ph = Math.min(procH - used, procH * Math.max(0, pt.dur_ms) / smp.server_ms);
          if (ph <= 0) return;
          s += rect(y0 - latH - used - ph, ph, partColor(pt.name));
          used += ph;
        });
        if (procH - used > 0.5) {
          s += rect(y0 - bh, procH - used, parts.length ? OTHER : partColor("traitement"));
        }
      } else {
        s += rect(y0 - bh, bh, smp.is_success ? "#4da3ff" : ERROR);
      }
    });
    s += '<text x="2" y="12">' + maxD + ' ms</text></svg>';
    html += s;
  });
  document.getElementById("percall").innerHTML = html;

  const sq = c => '<span style="display:inline-block;width:9px;height:9px;border-radius:2px;background:' + c + ';margin:0 4px 0 10px"></span>';
  let note;
  if (hasTiming) {
    note = sq(LATENCY) + 'latence (total - Server-Timing)';
    partColors.forEach((c, name) => { note += sq(c) + esc(name); });
    if (all.some(smp => (smp.server_parts || []).length)) note += sq(OTHER) + 'autre (reste du total)';
    note += sq(ERROR) + 'erreur';
  } else {
    note = "Barre = duree totale de l'appel. Exposez un en-tete Server-Timing pour separer latence et traitement.";
  }
  document.getElementById("percall-note").innerHTML = note;
})();

// ----- hover tooltip (strip plot + per-call bars) -----
(function () {
  const tip = document.createElement("div");
  tip.style.cssText = "position:absolute;display:none;pointer-events:none;z-index:10;" +
    "background:#0c1014;border:1px solid #2a313a;border-radius:6px;padding:8px 10px;" +
    "font:12px/1.6 Consolas,monospace;color:#e6e9ec;max-width:360px";
  document.body.appendChild(tip);

  function content(smp) {
    const rows = [];
    if (smp.name) rows.push('<span style="color:#4da3ff">' + esc(smp.name) + '</span>');
    rows.push('Total : ' + smp.duration_ms + ' ms (HTTP ' + (smp.status ?? "-") + ')');
    if (smp.server_ms != null) {
      rows.push('Latence : ' + Math.max(0, smp.duration_ms - smp.server_ms).toFixed(1) + ' ms');
      rows.push('Traitement : ' + smp.server_ms.toFixed(1) + ' ms');
      const parts = smp.server_parts || [];
      parts.forEach(pt =>
        rows.push('&nbsp;&nbsp;' + esc(pt.name) + ' : ' + pt.dur_ms.toFixed(1) + ' ms'));
      if (parts.length) {
        const rest = smp.server_ms - parts.reduce((a, pt) => a + pt.dur_ms, 0);
        if (rest > 0.05) rows.push('&nbsp;&nbsp;autre : ' + rest.toFixed(1) + ' ms');
      }
    }
    return rows.join('<br>');
  }

  document.addEventListener("mousemove", e => {
    const el = e.target.closest ? e.target.closest("[data-i]") : null;
    if (!el) { tip.style.display = "none"; return; }
    tip.innerHTML = content(phases[+el.dataset.p].samples[+el.dataset.i]);
    tip.style.display = "block";
    const x = Math.min(e.pageX + 14, document.documentElement.scrollWidth - tip.offsetWidth - 8);
    tip.style.left = x + "px";
    tip.style.top = (e.pageY + 14) + "px";
  });
})();

// ----- phase table -----
document.getElementById("phases").innerHTML =
  "<table><tr><th>Phase</th><th>Req</th><th>Err</th><th>Total</th>" +
  "<th>Mediane</th><th>P90</th><th>Max</th><th>Req/s</th></tr>" +
  phases.map(p => {
    const d = durations(p);
    return "<tr><td>" + label(p.concurrency) + "</td><td>" + p.samples.length +
      "</td><td>" + errors(p) + "</td><td>" + p.wall_ms + " ms</td><td>" +
      median(d) + " ms</td><td>" + p90(d) + " ms</td><td>" +
      (d[d.length - 1] ?? 0) + " ms</td><td>" + reqps(p).toFixed(1) + "</td></tr>";
  }).join("") + "</table>";

// ----- load profile over time (--load) -----
// Time-series line chart: `lines` are polylines, `band` an optional P90-P99
// ribbon, `marks` optional error dots; all in data coordinates (x seconds).
function lineChart(w, h, xMax, yMax, lines, band, marks, hover) {
  const pad = 34;
  const X = x => pad + (w - pad - 6) * (xMax ? x / xMax : 0);
  const Y = y => (h - 22) - (h - 34) * (yMax ? y / yMax : 0);
  let s = '<svg viewBox="0 0 ' + w + ' ' + h + '" width="100%">';
  if (band && band.lo.length) {
    const up = band.hi.map(p => X(p[0]) + ',' + Y(p[1]));
    const dn = band.lo.slice().reverse().map(p => X(p[0]) + ',' + Y(p[1]));
    s += '<polygon points="' + up.concat(dn).join(' ') + '" fill="' + band.color + '" fill-opacity="0.18"/>';
  }
  lines.forEach(ln => {
    const pts = ln.points.map(p => X(p[0]) + ',' + Y(p[1])).join(' ');
    s += '<polyline points="' + pts + '" fill="none" stroke="' + ln.color + '" stroke-width="' + (ln.width || 2) + '"/>';
    ln.points.forEach(p => { s += '<circle cx="' + X(p[0]) + '" cy="' + Y(p[1]) + '" r="1.6" fill="' + ln.color + '"/>'; });
  });
  (marks || []).forEach(m => { s += '<circle cx="' + X(m[0]) + '" cy="' + Y(m[1]) + '" r="3" fill="#e05555"/>'; });
  s += '<text x="2" y="12">' + Math.round(yMax) + '</text>';
  s += '<text x="' + (w - 4) + '" y="' + (h - 4) + '" text-anchor="end">' + Math.round(xMax) + ' s</text>';
  // Transparent full-height hit columns, one per x, drawn last so they receive
  // the hover; the mousemove handler reads data-load/data-sec to build the tip.
  if (hover) {
    const colW = (hover.xs.length > 1) ? (X(hover.xs[1]) - X(hover.xs[0])) : (w - pad - 6);
    hover.xs.forEach(x => {
      s += '<rect x="' + (X(x) - colW / 2) + '" y="0" width="' + Math.max(1, colW) + '" height="' + h +
           '" fill="#fff" fill-opacity="0" pointer-events="all" data-load="' + hover.id + '" data-sec="' + x + '"/>';
    });
  }
  return s + '</svg>';
}
if (DATA.report.load) {
  const L = DATA.report.load, B = L.buckets;
  const xMax = B.length ? B[B.length - 1].second : 0;
  const secs = B.map(b => b.second);
  const tpMax = Math.max(1, ...B.map(b => Math.max(b.target_cps, b.completed)));
  document.getElementById("load-throughput").innerHTML = lineChart(1000, 220, xMax, tpMax,
    [{ points: B.map(b => [b.second, b.target_cps]), color: "#8a94a0", width: 2 },
     { points: B.map(b => [b.second, b.completed]), color: "#3fb96f", width: 2 }],
    null, null, { id: "tp", xs: secs });
  const rtMax = Math.max(1, ...B.map(b => b.p99_ms));
  document.getElementById("load-latency").innerHTML = lineChart(1000, 220, xMax, rtMax,
    [{ points: B.map(b => [b.second, b.p50_ms]), color: "#4da3ff", width: 2 }],
    { lo: B.map(b => [b.second, b.p90_ms]), hi: B.map(b => [b.second, b.p99_ms]), color: "#4da3ff" },
    B.filter(b => b.errors > 0).map(b => [b.second, b.p50_ms]), { id: "rt", xs: secs });
  const sent = B.reduce((a, b) => a + b.sent, 0);
  const done = B.reduce((a, b) => a + b.completed, 0);
  const errs = B.reduce((a, b) => a + b.errors, 0);
  document.getElementById("load-stages").innerHTML =
    "<table><tr><th>#</th><th>Forme</th><th>Duree</th><th>Cible CPS</th></tr>" +
    L.stages.map((st, i) => "<tr><td>" + (i + 1) + "</td><td>" + esc(st.shape) + "</td><td>" +
      esc(st.duration) + "</td><td>" + (st.target_cps ?? "-") + "</td></tr>").join("") + "</table>" +
    "<div class='note'>Envoyees " + sent + " | Terminees " + done + " | Erreurs " + errs +
    (L.stop_reason ? " | Arret : " + esc(L.stop_reason) : "") + "</div>";

  // Cursor tooltip: the numbers at the hovered second, per chart.
  const bySec = new Map(B.map(b => [b.second, b]));
  const loadTip = document.createElement("div");
  loadTip.style.cssText = "position:absolute;display:none;pointer-events:none;z-index:10;" +
    "background:#0c1014;border:1px solid #2a313a;border-radius:6px;padding:8px 10px;" +
    "font:12px/1.6 Consolas,monospace;color:#e6e9ec";
  document.body.appendChild(loadTip);
  document.addEventListener("mousemove", e => {
    const el = e.target.closest ? e.target.closest("[data-load]") : null;
    const b = el ? bySec.get(+el.dataset.sec) : null;
    if (!b) { loadTip.style.display = "none"; return; }
    const rows = ['<span style="color:#4da3ff">t = ' + b.second + ' s</span>'];
    if (el.dataset.load === "tp") {
      rows.push("cible : " + b.target_cps.toFixed(1) + " req/s");
      rows.push("atteint : " + b.completed + " req/s");
      rows.push("envoi : " + b.sent);
    } else {
      rows.push("mediane : " + b.p50_ms + " ms");
      rows.push("P90 : " + b.p90_ms + " ms");
      rows.push("P99 : " + b.p99_ms + " ms");
      if (b.errors) rows.push('<span style="color:#e05555">erreurs : ' + b.errors + "</span>");
    }
    loadTip.innerHTML = rows.join("<br>");
    loadTip.style.display = "block";
    const x = Math.min(e.pageX + 14, document.documentElement.scrollWidth - loadTip.offsetWidth - 8);
    loadTip.style.left = x + "px";
    loadTip.style.top = (e.pageY + 14) + "px";
  });

  document.getElementById("load-section").style.display = "";
}

// ----- per-step metrics (--metrics) -----
renderMetrics(DATA.meta.metrics, document.getElementById("metrics"));

__METRICS_JS__
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use vantage_core::benchmark::{BenchReport, BenchSample, PhaseReport};

    fn sample(duration_ms: u128, success: bool) -> BenchSample {
        BenchSample {
            name: "getPrice - A".to_string(),
            offset_ms: 0,
            duration_ms,
            status: Some(if success { 200 } else { 500 }),
            is_success: success,
            server_ms: None,
            server_parts: vec![],
        }
    }

    fn load_sample(offset_ms: u128, duration_ms: u128, success: bool) -> BenchSample {
        let mut s = sample(duration_ms, success);
        s.offset_ms = offset_ms;
        s
    }

    fn load_report_for(spec: &str, samples: Vec<BenchSample>, stop: Option<&str>) -> BenchReport {
        let profile = vantage_core::load::LoadProfile::parse(spec).unwrap();
        let load =
            vantage_core::benchmark::LoadReport::new(&profile, samples, stop.map(str::to_string));
        BenchReport {
            warmup_count: 0,
            pool_size: 4,
            latency: None,
            phases: vec![],
            stop_reason: None,
            load: Some(load),
        }
    }

    #[test]
    fn report_contains_phases_stop_reason_and_split() {
        let report = BenchReport {
            warmup_count: 2,
            pool_size: 3,
            latency: None,
            phases: vec![
                PhaseReport {
                    concurrency: 1,
                    wall_ms: 300,
                    samples: vec![sample(100, true), sample(110, true), sample(90, false)],
                },
                PhaseReport {
                    concurrency: 2,
                    wall_ms: 160,
                    samples: vec![sample(100, true), sample(120, true), sample(95, true)],
                },
            ],
            stop_reason: Some("HTTP 429 received at concurrency 2".to_string()),
            load: None,
        };

        let text = format_report("suite.json", &report);

        assert!(text.contains("pool : 3 requetes, warm-up : 2"), "{text}");
        assert!(text.contains("sequentiel"), "{text}");
        assert!(text.contains("parallele x2"), "{text}");
        assert!(text.contains("Arret : HTTP 429"), "{text}");
        assert!(text.contains("Succes : 5 req"), "{text}");
        assert!(text.contains("Erreurs : 1 req"), "{text}");
    }

    #[test]
    fn data_json_carries_the_per_call_server_timing() {
        let mut with_timing = sample(50, true);
        with_timing.server_ms = Some(41.5);
        with_timing.server_parts = vec![
            vantage_core::benchmark::ServerSpan {
                name: "app".to_string(),
                dur_ms: 30.0,
            },
            vantage_core::benchmark::ServerSpan {
                name: "db".to_string(),
                dur_ms: 10.0,
            },
        ];
        let report = BenchReport {
            warmup_count: 0,
            pool_size: 2,
            latency: None,
            phases: vec![PhaseReport {
                concurrency: 1,
                wall_ms: 100,
                samples: vec![with_timing, sample(60, true)],
            }],
            stop_reason: None,
            load: None,
        };

        let root = std::env::temp_dir().join(format!("vantage_bench_st_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir =
            write_report_files("s.json", "test", &report, serde_json::Value::Null, &root).unwrap();

        let raw = std::fs::read_to_string(dir.join("data.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let samples = &parsed["report"]["phases"][0]["samples"];
        assert_eq!(samples[0]["server_ms"], 41.5);
        assert_eq!(samples[0]["server_parts"][0]["name"], "app");
        assert_eq!(samples[0]["server_parts"][1]["dur_ms"], 10.0);
        assert!(
            samples[1].get("server_ms").is_none(),
            "absent Server-Timing must not serialize a null"
        );
        assert!(
            samples[1].get("server_parts").is_none(),
            "no components -> no empty array in the data"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn latency_probe_side_without_samples_shows_na() {
        let report = BenchReport {
            warmup_count: 0,
            pool_size: 1,
            latency: Some(vantage_core::benchmark::LatencyReport {
                tcp_us: vec![],
                http_us: vec![8000],
            }),
            phases: vec![PhaseReport {
                concurrency: 1,
                wall_ms: 100,
                samples: vec![sample(50, true)],
            }],
            stop_reason: None,
            load: None,
        };

        let text = format_report("suite.json", &report);

        assert!(text.contains("TCP n/a"), "{text}");
        assert!(text.contains("HTTP ~8.0ms"), "{text}");
    }

    #[test]
    fn embedded_data_cannot_break_out_of_the_inline_script() {
        let mut hostile = sample(50, true);
        hostile.name = "x</script><script>alert(1)</script>".to_string();
        let report = BenchReport {
            warmup_count: 0,
            pool_size: 1,
            latency: None,
            phases: vec![PhaseReport {
                concurrency: 1,
                wall_ms: 100,
                samples: vec![hostile],
            }],
            stop_reason: None,
            load: None,
        };

        let root = std::env::temp_dir().join(format!("vantage_bench_esc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir =
            write_report_files("s.json", "test", &report, serde_json::Value::Null, &root).unwrap();

        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(
            !html.contains("x</script>"),
            "embedded JSON must not be able to close the script tag"
        );
        assert!(
            html.contains("esc(name)"),
            "the per-call legend must escape server-provided part names"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn report_shows_the_latency_probe_when_present() {
        let report = BenchReport {
            warmup_count: 0,
            pool_size: 2,
            latency: Some(vantage_core::benchmark::LatencyReport {
                tcp_us: vec![2100, 1900, 2000],
                http_us: vec![8400, 8600, 8500],
            }),
            phases: vec![PhaseReport {
                concurrency: 1,
                wall_ms: 100,
                samples: vec![sample(50, true), sample(50, true)],
            }],
            stop_reason: None,
            load: None,
        };

        let text = format_report("suite.json", &report);

        assert!(text.contains("Latence reseau"), "{text}");
        assert!(text.contains("TCP ~2.0ms"), "{text}");
        assert!(text.contains("HTTP ~8.5ms"), "{text}");
    }

    #[test]
    fn writes_data_json_and_self_contained_html() {
        let report = BenchReport {
            warmup_count: 1,
            pool_size: 2,
            latency: None,
            phases: vec![PhaseReport {
                concurrency: 1,
                wall_ms: 200,
                samples: vec![sample(100, true), sample(95, false)],
            }],
            stop_reason: None,
            load: None,
        };

        let root =
            std::env::temp_dir().join(format!("vantage_bench_report_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let dir = write_report_files(
            "test-suite/mySuite.json",
            "Sandbox",
            &report,
            serde_json::Value::Null,
            &root,
        )
        .unwrap();

        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mySuite_"),
            "{dir:?}"
        );

        let raw = std::fs::read_to_string(dir.join("data.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["meta"]["environment"], "Sandbox");
        assert_eq!(
            parsed["report"]["phases"][0]["samples"][0]["duration_ms"],
            100
        );

        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(!html.contains("__DATA__"), "placeholder must be replaced");
        assert!(
            html.contains("Durees par appel"),
            "the per-call section must be present"
        );
        assert!(
            html.contains("server_parts"),
            "the per-call chart must render the Server-Timing components"
        );
        assert!(
            html.contains("data-i") && html.contains("mousemove"),
            "bars and points must carry sample refs for the hover tooltip"
        );
        assert!(
            html.contains("const esc"),
            "the tooltip escapes call names and must define its own helper"
        );
        assert!(html.contains("\"pool_size\""), "data must be embedded");
        assert!(
            html.contains("<svg") || html.contains("barChart"),
            "charts expected"
        );
        assert!(
            !html.contains("http://cdn") && !html.contains("https://cdn"),
            "offline report"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn benchmark_report_embeds_metrics_when_provided() {
        let report = BenchReport {
            warmup_count: 0,
            pool_size: 1,
            latency: None,
            phases: vec![PhaseReport {
                concurrency: 1,
                wall_ms: 50,
                samples: vec![sample(50, true)],
            }],
            stop_reason: None,
            load: None,
        };
        let metrics = serde_json::json!([{
            "name": "http", "count": 1, "total_ms": 50.0, "median_ms": 50.0,
            "p90_ms": 50.0, "expected_ms": 250.0, "description": "Network round-trip"
        }]);

        let root =
            std::env::temp_dir().join(format!("vantage_bench_metrics_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = write_report_files("s.json", "test", &report, metrics, &root).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert_eq!(parsed["meta"]["metrics"][0]["name"], "http");

        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(
            html.contains("Performance par etape") && html.contains("renderMetrics"),
            "the benchmark report must include the metrics section"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn format_report_summarizes_the_load_profile() {
        let report = load_report_for(
            "step:3s:10",
            vec![
                load_sample(0, 100, true),
                load_sample(1000, 100, true),
                load_sample(1100, 100, false),
            ],
            Some("HTTP 429 during the load profile"),
        );
        let text = format_report("suite.json", &report);
        assert!(text.contains("Profil de charge : 1 etape(s)"), "{text}");
        assert!(
            text.contains("step"),
            "the per-stage roll-up must list the stage: {text}"
        );
        assert!(
            text.contains("Total : 3 envoyees, 3 terminees, 1 erreurs"),
            "{text}"
        );
        assert!(text.contains("Arret (charge) : HTTP 429"), "{text}");
    }

    #[test]
    fn html_and_data_carry_the_load_section() {
        let report = load_report_for(
            "ramp:2s:8,hold:1s",
            vec![
                load_sample(0, 50, true),
                load_sample(1500, 60, true),
                load_sample(2500, 70, false),
            ],
            None,
        );
        let root = std::env::temp_dir().join(format!("vantage_bench_load_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir =
            write_report_files("s.json", "test", &report, serde_json::Value::Null, &root).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert!(
            parsed["report"]["load"]["buckets"]
                .as_array()
                .is_some_and(|b| !b.is_empty()),
            "data.json must carry the per-second load buckets"
        );
        assert_eq!(parsed["report"]["load"]["stages"][0]["shape"], "ramp");

        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(html.contains("load-section"), "the load section markup");
        assert!(
            html.contains("function lineChart"),
            "the time-series chart helper"
        );
        assert!(
            html.contains("Debit dans le temps"),
            "throughput-over-time graph"
        );
        assert!(
            html.contains("Temps de reponse dans le temps"),
            "response-time-over-time graph"
        );
        assert!(
            html.contains("data-load=")
                && html.contains(r#"id: "tp""#)
                && html.contains(r#"id: "rt""#),
            "both time-series charts must emit hover hit-columns"
        );
        assert!(
            html.contains("loadTip"),
            "a cursor tooltip must read the hovered bucket's numbers"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_run_without_a_load_profile_omits_it_from_the_data() {
        let report = BenchReport {
            warmup_count: 0,
            pool_size: 1,
            latency: None,
            phases: vec![PhaseReport {
                concurrency: 1,
                wall_ms: 50,
                samples: vec![sample(50, true)],
            }],
            stop_reason: None,
            load: None,
        };
        let root =
            std::env::temp_dir().join(format!("vantage_bench_noload_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir =
            write_report_files("s.json", "test", &report, serde_json::Value::Null, &root).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("data.json")).unwrap()).unwrap();
        assert!(
            parsed["report"].get("load").is_none(),
            "escalation-only runs carry no load key"
        );
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(html.contains(r#"id="load-section" style="display:none""#));

        let _ = std::fs::remove_dir_all(&root);
    }
}
