//! Shared `--metrics` panel for the HTML reports.
//!
//! Both the run report ([`crate::html`]) and the benchmark report
//! ([`crate::benchmark`]) embed the same per-step metrics section (bar chart
//! and budget table). The JS renderer lives here once, spliced into each
//! template in place of the `__METRICS_JS__` placeholder, so the two reports
//! cannot drift apart.

/// JS function `renderMetrics(steps, host)`: renders the per-step bar chart
/// and the budget table from the `--metrics` JSON embedded in the report.
/// Amber marks a step whose median exceeds its expected per-call budget.
pub(crate) const METRICS_PANEL_JS: &str = r##"function renderMetrics(steps, host) {
  if (!host || !Array.isArray(steps) || !steps.length) return;
  const esc2 = s => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/"/g, "&quot;");
  const max = Math.max(...steps.map(s => s.median_ms), 1);
  const bw = 30, gap = 16, h = 170;
  const W = Math.max(320, steps.length * (bw + gap) + 20);
  let svg = '<svg viewBox="0 0 ' + W + ' ' + h + '" width="100%" style="max-width:' + W + 'px">';
  steps.forEach((s, i) => {
    const x = 12 + i * (bw + gap);
    const bh = (h - 34) * s.median_ms / max;
    const over = s.expected_ms != null && s.median_ms > s.expected_ms;
    svg += '<rect x="' + x + '" y="' + (h - 20 - bh) + '" width="' + bw + '" height="' + Math.max(1, bh) +
      '" rx="2" fill="' + (over ? "#e0a14d" : "#4da3ff") + '"><title>' + esc2(s.name) + " median " +
      s.median_ms.toFixed(1) + ' ms</title></rect>';
    svg += '<text x="' + (x + bw / 2) + '" y="' + (h - 6) + '" text-anchor="middle">' + esc2(s.name) + '</text>';
  });
  svg += '<text x="2" y="12">' + max.toFixed(0) + ' ms</text></svg>';
  const rows = steps.map(s => {
    const over = s.expected_ms != null && s.median_ms > s.expected_ms;
    const exp = s.expected_ms != null ? ("≤ " + s.expected_ms + " ms") : "-";
    const budget = s.expected_ms != null
      ? " (expected ≤ " + s.expected_ms + " ms per call)" : " (no fixed budget)";
    const tip = esc2(s.description + budget);
    return '<tr' + (over ? ' class="over"' : '') + '><td>' + esc2(s.name) +
      ' <span class="info" tabindex="0" data-tip="' + tip + '">ⓘ</span></td><td>' +
      s.count + '</td><td>' + s.total_ms.toFixed(1) + ' ms</td><td>' + s.median_ms.toFixed(1) +
      ' ms</td><td>' + s.p90_ms.toFixed(1) + ' ms</td><td>' + exp + '</td></tr>';
  }).join("");
  host.innerHTML = '<div class="metrics-sec"><h2>Performance par etape (--metrics)</h2>' + svg +
    '<table><tr><th>Etape</th><th>n</th><th>Total</th><th>Mediane</th><th>P90</th><th>Attendu</th></tr>' +
    rows + '</table><div class="note">Mediane par appel ; ambre = au-dessus du budget attendu. ' +
    'Duree = cycle de vie complet du span (l\'attente comprise) ; sous concurrence les totaux ' +
    'peuvent depasser le temps mur.</div></div>';
}"##;
