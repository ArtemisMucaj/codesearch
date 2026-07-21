//! Render a [`GraphView`] into shareable visual artifacts.
//!
//! Ports three of the exporters from the sibling `graphify` project:
//!   * **HTML** — a self-contained interactive [vis-network] page (community
//!     colours, search, per-community filtering, click-to-inspect, physics).
//!   * **SVG** — a static image laid out with a small deterministic
//!     Fruchterman–Reingold force simulation; embeds in Markdown/READMEs.
//!   * **Canvas** — an Obsidian `.canvas` JSON, communities laid out as groups.
//!
//! All three colour nodes by their Leiden community using a shared palette.
//! Everything here is a pure function of the [`GraphView`]; writing the output
//! to disk is the connector's job.
//!
//! [vis-network]: https://visjs.github.io/vis-network/

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{json, Value};

use crate::domain::{GraphEdge, GraphNode, GraphView, NAMESPACE_SCOPE_ID};

/// Human-readable label for a view's scope id. The namespace-wide sentinel is an
/// internal cache key (`__namespace__` / `__namespace__:<ns>`) and must never
/// surface to the user: a bare sentinel renders as `namespace`, and a
/// namespace-qualified one as `namespace <ns>`. Any real repository id is shown
/// verbatim.
fn scope_label(repository_id: &str) -> String {
    if repository_id == NAMESPACE_SCOPE_ID {
        "namespace".to_string()
    } else if let Some(ns) = repository_id.strip_prefix(&format!("{NAMESPACE_SCOPE_ID}:")) {
        format!("namespace {ns}")
    } else {
        repository_id.to_string()
    }
}

/// Tableau-10 palette, cycled per community index. Matches graphify so exported
/// HTML/SVG/canvas share a colour scheme.
pub const COMMUNITY_COLORS: [&str; 10] = [
    "#4E79A7", "#F28E2B", "#E15759", "#76B7B2", "#59A14F", "#EDC948", "#B07AA1", "#FF9DA7",
    "#9C755F", "#BAB0AC",
];

/// Above this many nodes, an interactive/positional view becomes an unreadable
/// hairball; callers collapse to the aggregated community meta-graph instead.
pub const DEFAULT_NODE_LIMIT: usize = 5000;

/// Which artifact to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VizFormat {
    Html,
    Svg,
    Canvas,
}

impl VizFormat {
    /// File extension (without the dot) conventionally used for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            VizFormat::Html => "html",
            VizFormat::Svg => "svg",
            VizFormat::Canvas => "canvas",
        }
    }
}

/// Render `view` into the chosen format, returning the file contents.
pub fn render(view: &GraphView, format: VizFormat) -> String {
    match format {
        VizFormat::Html => render_html(view),
        VizFormat::Svg => render_svg(view),
        VizFormat::Canvas => render_canvas(view),
    }
}

/// The colour for a given community index.
fn color_for(community: usize) -> &'static str {
    COMMUNITY_COLORS[community % COMMUNITY_COLORS.len()]
}

/// Look up a community's display name, falling back to `Community N`.
fn community_name(view: &GraphView, index: usize) -> String {
    view.communities
        .iter()
        .find(|c| c.index == index)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| format!("Community {}", index))
}

// ── Aggregation (large-graph fallback) ─────────────────────────────────────

/// Collapse `view` into a community meta-graph: one node per community (sized by
/// member count), with edges weighted by the number of cross-community edges
/// between each pair. Mirrors graphify's auto-aggregation for oversized graphs.
pub fn aggregate(view: &GraphView) -> GraphView {
    // Position of each community index within the (size-sorted) list, so the
    // meta-node order matches the legend.
    let mut pos_of: BTreeMap<usize, usize> = BTreeMap::new();
    for (pos, c) in view.communities.iter().enumerate() {
        pos_of.insert(c.index, pos);
    }

    let nodes: Vec<GraphNode> = view
        .communities
        .iter()
        .map(|c| GraphNode {
            id: format!("community-{}", c.index),
            label: c.name.clone(),
            community: c.index,
            // Size meta-nodes by member count rather than meta-edge degree.
            degree: c.size,
            language: String::new(),
            // A community meta-node spans its members (possibly many repos).
            repository: None,
        })
        .collect();

    let mut counts: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for edge in &view.edges {
        let cu = view.nodes[edge.source].community;
        let cv = view.nodes[edge.target].community;
        if cu == cv {
            continue;
        }
        let (Some(&a), Some(&b)) = (pos_of.get(&cu), pos_of.get(&cv)) else {
            continue;
        };
        let key = if a < b { (a, b) } else { (b, a) };
        *counts.entry(key).or_insert(0.0) += 1.0;
    }

    let edges: Vec<GraphEdge> = counts
        .into_iter()
        .map(|((source, target), weight)| GraphEdge {
            source,
            target,
            weight,
            kind: None,
        })
        .collect();

    GraphView {
        repository_id: view.repository_id.clone(),
        level: view.level,
        nodes,
        edges,
        communities: view.communities.clone(),
    }
}

// ── Escaping helpers ────────────────────────────────────────────────────────

/// Escape text for safe inclusion in HTML/XML element content and attributes.
fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Serialize to JSON, neutralising any `</script>` sequence so embedded data
/// cannot break out of a `<script>` tag.
fn js_safe(value: &Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "[]".to_string())
        .replace("</", "<\\/")
}

// ── HTML (vis-network) ──────────────────────────────────────────────────────

const HTML_STYLES: &str = r#"<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: #0f0f1a; color: #e0e0e0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; display: flex; height: 100vh; overflow: hidden; }
  #graph { flex: 1; }
  #sidebar { width: 300px; background: #1a1a2e; border-left: 1px solid #2a2a4e; display: flex; flex-direction: column; overflow: hidden; }
  #search-wrap { padding: 12px; border-bottom: 1px solid #2a2a4e; }
  #search { width: 100%; background: #0f0f1a; border: 1px solid #3a3a5e; color: #e0e0e0; padding: 7px 10px; border-radius: 6px; font-size: 13px; outline: none; }
  #search:focus { border-color: #4E79A7; }
  #search-results { max-height: 140px; overflow-y: auto; padding: 4px 0; display: none; }
  .search-item { padding: 4px 6px; cursor: pointer; border-radius: 4px; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .search-item:hover { background: #2a2a4e; }
  #info-panel { padding: 14px; border-bottom: 1px solid #2a2a4e; min-height: 120px; }
  #info-panel h3, #legend-wrap h3 { font-size: 13px; color: #aaa; margin-bottom: 8px; text-transform: uppercase; letter-spacing: 0.05em; }
  #info-content { font-size: 13px; color: #ccc; line-height: 1.6; word-break: break-word; }
  #info-content .field b { color: #e0e0e0; }
  #info-content .empty { color: #555; font-style: italic; }
  .neighbor-link { display: block; padding: 2px 6px; margin: 2px 0; border-radius: 3px; cursor: pointer; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; border-left: 3px solid #333; }
  .neighbor-link:hover { background: #2a2a4e; }
  #neighbors { max-height: 160px; overflow-y: auto; margin-top: 6px; }
  #legend-wrap { flex: 1; overflow-y: auto; padding: 12px; }
  .legend-item { display: flex; align-items: center; gap: 8px; padding: 4px 0; cursor: pointer; border-radius: 4px; font-size: 12px; }
  .legend-item:hover { background: #2a2a4e; }
  .legend-item.dimmed { opacity: 0.35; }
  .legend-dot { width: 12px; height: 12px; border-radius: 50%; flex-shrink: 0; }
  .legend-label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .legend-count { color: #666; font-size: 11px; }
  #legend-controls { margin-bottom: 8px; }
  #legend-controls label { display: flex; align-items: center; gap: 6px; cursor: pointer; font-size: 12px; color: #aaa; }
  #stats { padding: 10px 14px; border-top: 1px solid #2a2a4e; font-size: 11px; color: #555; }
</style>"#;

const HTML_SCRIPT_TAG: &str = "<script src=\"https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js\"></script>\n";

const HTML_JS: &str = r#"
function esc(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;');
}
const nodesDS = new vis.DataSet(RAW_NODES.map(n => ({
  id: n.id, label: n.label, color: n.color, size: n.size, font: n.font, title: n.title,
  _community: n.community, _community_name: n.community_name, _language: n.language, _degree: n.degree,
})));
const edgesDS = new vis.DataSet(RAW_EDGES.map((e, i) => ({
  id: i, from: e.from, to: e.to, title: e.title, dashes: e.dashes, width: e.width, color: e.color,
  arrows: { to: { enabled: true, scaleFactor: 0.5 } },
})));
const container = document.getElementById('graph');
const network = new vis.Network(container, { nodes: nodesDS, edges: edgesDS }, {
  physics: { enabled: true, solver: 'forceAtlas2Based',
    forceAtlas2Based: { gravitationalConstant: -60, centralGravity: 0.005, springLength: 120, springConstant: 0.08, damping: 0.4, avoidOverlap: 0.8 },
    stabilization: { iterations: 200, fit: true } },
  interaction: { hover: true, tooltipDelay: 100, hideEdgesOnDrag: true },
  nodes: { shape: 'dot', borderWidth: 1.5 },
  edges: { smooth: { type: 'continuous', roundness: 0.2 }, selectionWidth: 3 },
});
network.once('stabilizationIterationsDone', () => network.setOptions({ physics: { enabled: false } }));

function showInfo(nodeId) {
  const n = nodesDS.get(nodeId);
  if (!n) return;
  const neighbors = network.getConnectedNodes(nodeId).map(nid => {
    const nb = nodesDS.get(nid);
    const c = nb ? nb.color.background : '#555';
    return '<span class="neighbor-link" style="border-left-color:' + esc(c) + '" onclick="focusNode(' + JSON.stringify(nid) + ')">' + esc(nb ? nb.label : nid) + '</span>';
  }).join('');
  document.getElementById('info-content').innerHTML =
    '<div class="field"><b>' + esc(n.label) + '</b></div>' +
    '<div class="field">Community: ' + esc(n._community_name) + '</div>' +
    '<div class="field">Language: ' + esc(n._language || '—') + '</div>' +
    '<div class="field">Connections: ' + esc(n._degree) + '</div>' +
    (neighbors ? '<div id="neighbors">' + neighbors + '</div>' : '');
}
function focusNode(nodeId) {
  network.selectNodes([nodeId]);
  network.focus(nodeId, { scale: 1.2, animation: true });
  showInfo(nodeId);
}
network.on('click', p => {
  if (p.nodes.length) showInfo(p.nodes[0]);
  else document.getElementById('info-content').innerHTML = '<span class="empty">Click a node to inspect it</span>';
});

const searchBox = document.getElementById('search');
const searchResults = document.getElementById('search-results');
searchBox.addEventListener('input', () => {
  const q = searchBox.value.toLowerCase().trim();
  if (!q) { searchResults.style.display = 'none'; return; }
  const hits = RAW_NODES.filter(n => n.label.toLowerCase().includes(q) || String(n.id).toLowerCase().includes(q)).slice(0, 30);
  searchResults.innerHTML = hits.map(n => '<div class="search-item" onclick="focusNode(' + JSON.stringify(n.id) + ')">' + esc(n.label) + '</div>').join('');
  searchResults.style.display = hits.length ? 'block' : 'none';
});

const hidden = new Set();
function applyHidden() {
  nodesDS.update(RAW_NODES.map(n => ({ id: n.id, hidden: hidden.has(n.community) })));
}
function renderLegend() {
  const el = document.getElementById('legend');
  el.innerHTML = LEGEND.map(c =>
    '<div class="legend-item" data-cid="' + c.cid + '">' +
    '<span class="legend-dot" style="background:' + esc(c.color) + '"></span>' +
    '<span class="legend-label">' + esc(c.label) + '</span>' +
    '<span class="legend-count">' + c.count + '</span></div>').join('');
  el.querySelectorAll('.legend-item').forEach(item => {
    item.addEventListener('click', () => {
      const cid = parseInt(item.getAttribute('data-cid'), 10);
      if (hidden.has(cid)) { hidden.delete(cid); item.classList.remove('dimmed'); }
      else { hidden.add(cid); item.classList.add('dimmed'); }
      applyHidden();
      document.getElementById('select-all-cb').checked = hidden.size === 0;
    });
  });
}
renderLegend();
document.getElementById('select-all-cb').addEventListener('change', e => {
  hidden.clear();
  if (!e.target.checked) LEGEND.forEach(c => hidden.add(c.cid));
  applyHidden();
  document.querySelectorAll('.legend-item').forEach(it => it.classList.toggle('dimmed', !e.target.checked));
});
"#;

fn render_html(view: &GraphView) -> String {
    let max_deg = view
        .nodes
        .iter()
        .map(|n| n.degree)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let vis_nodes: Vec<Value> = view
        .nodes
        .iter()
        .map(|n| {
            let color = color_for(n.community);
            let deg = n.degree as f64;
            let size = 10.0 + 30.0 * (deg / max_deg);
            let font_size = if deg >= max_deg * 0.15 { 12 } else { 0 };
            json!({
                "id": n.id,
                "label": n.label,
                "color": { "background": color, "border": color, "highlight": { "background": "#ffffff", "border": color } },
                "size": (size * 10.0).round() / 10.0,
                "font": { "size": font_size, "color": "#ffffff" },
                "title": n.id,
                "community": n.community,
                "community_name": community_name(view, n.community),
                "language": n.language,
                "degree": n.degree,
            })
        })
        .collect();

    let vis_edges: Vec<Value> = view
        .edges
        .iter()
        .map(|e| {
            let strong = matches!(e.kind.as_deref(), Some("call") | Some("methodcall"));
            let dashed = !strong && e.kind.is_some();
            let title = match &e.kind {
                Some(k) => format!("{} (weight {:.1})", k, e.weight),
                None => format!("weight {:.1}", e.weight),
            };
            json!({
                "from": view.nodes[e.source].id,
                "to": view.nodes[e.target].id,
                "title": title,
                "dashes": dashed,
                "width": if strong { 2 } else { 1 },
                "color": { "opacity": if strong { 0.7 } else { 0.4 } },
            })
        })
        .collect();

    let legend: Vec<Value> = view
        .communities
        .iter()
        .map(|c| {
            json!({ "cid": c.index, "color": color_for(c.index), "label": c.name, "count": c.size })
        })
        .collect();

    let stats = format!(
        "{} {}s · {} edges · {} communities",
        view.node_count(),
        view.level.node_noun(),
        view.edge_count(),
        view.communities.len(),
    );

    let mut s = String::new();
    s.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"UTF-8\">\n<title>codesearch — ");
    s.push_str(&escape_xml(&scope_label(&view.repository_id)));
    s.push_str("</title>\n");
    s.push_str(HTML_SCRIPT_TAG);
    s.push_str(HTML_STYLES);
    s.push_str("\n</head>\n<body>\n<div id=\"graph\"></div>\n<div id=\"sidebar\">\n");
    s.push_str("  <div id=\"search-wrap\"><input id=\"search\" type=\"text\" placeholder=\"Search nodes…\" autocomplete=\"off\"><div id=\"search-results\"></div></div>\n");
    s.push_str("  <div id=\"info-panel\"><h3>Node Info</h3><div id=\"info-content\"><span class=\"empty\">Click a node to inspect it</span></div></div>\n");
    s.push_str("  <div id=\"legend-wrap\"><h3>Communities</h3><div id=\"legend-controls\"><label><input type=\"checkbox\" id=\"select-all-cb\" checked>Select all</label></div><div id=\"legend\"></div></div>\n");
    s.push_str("  <div id=\"stats\">");
    s.push_str(&escape_xml(&stats));
    s.push_str("</div>\n</div>\n<script>\n");
    s.push_str("const RAW_NODES = ");
    s.push_str(&js_safe(&Value::Array(vis_nodes)));
    s.push_str(";\nconst RAW_EDGES = ");
    s.push_str(&js_safe(&Value::Array(vis_edges)));
    s.push_str(";\nconst LEGEND = ");
    s.push_str(&js_safe(&Value::Array(legend)));
    s.push_str(";\n");
    s.push_str(HTML_JS);
    s.push_str("</script>\n</body>\n</html>\n");
    s
}

// ── Force-directed layout (Fruchterman–Reingold) ────────────────────────────

/// Deterministic SplitMix64 — seeds initial node positions reproducibly so the
/// same graph always lays out the same way.
struct Rng(u64);
impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let z = z ^ (z >> 31);
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Run a small Fruchterman–Reingold simulation, returning positions normalised
/// into `[0, 1]²`. Deterministic for a given graph.
fn layout(view: &GraphView) -> Vec<(f64, f64)> {
    let n = view.nodes.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![(0.5, 0.5)];
    }

    let mut rng = Rng(0x5EED_1DEA_C0DE_F00D);
    let mut pos: Vec<(f64, f64)> = (0..n).map(|_| (rng.next_f64(), rng.next_f64())).collect();

    let area = 1.0_f64;
    let k = (area / n as f64).sqrt();
    let iterations = if n > 500 { 60 } else { 120 };
    let mut temp = 0.1_f64;
    let cooling = temp / (iterations as f64 + 1.0);

    for _ in 0..iterations {
        let mut disp = vec![(0.0_f64, 0.0_f64); n];

        // Repulsive forces between every pair.
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = pos[i].0 - pos[j].0;
                let dy = pos[i].1 - pos[j].1;
                let dist = (dx * dx + dy * dy).sqrt().max(1e-4);
                let force = k * k / dist;
                let (ux, uy) = (dx / dist, dy / dist);
                disp[i].0 += ux * force;
                disp[i].1 += uy * force;
                disp[j].0 -= ux * force;
                disp[j].1 -= uy * force;
            }
        }

        // Attractive forces along edges.
        for e in &view.edges {
            let (a, b) = (e.source, e.target);
            let dx = pos[a].0 - pos[b].0;
            let dy = pos[a].1 - pos[b].1;
            let dist = (dx * dx + dy * dy).sqrt().max(1e-4);
            let force = dist * dist / k;
            let (ux, uy) = (dx / dist, dy / dist);
            disp[a].0 -= ux * force;
            disp[a].1 -= uy * force;
            disp[b].0 += ux * force;
            disp[b].1 += uy * force;
        }

        // Limit displacement by temperature, keep inside the unit square.
        for i in 0..n {
            let d = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1)
                .sqrt()
                .max(1e-4);
            let capped = d.min(temp);
            pos[i].0 = (pos[i].0 + disp[i].0 / d * capped).clamp(0.0, 1.0);
            pos[i].1 = (pos[i].1 + disp[i].1 / d * capped).clamp(0.0, 1.0);
        }
        temp -= cooling;
    }
    pos
}

// ── SVG ─────────────────────────────────────────────────────────────────────

fn render_svg(view: &GraphView) -> String {
    const W: f64 = 1600.0;
    const H: f64 = 1100.0;
    const MARGIN: f64 = 60.0;

    let pos = layout(view);
    let max_deg = view
        .nodes
        .iter()
        .map(|n| n.degree)
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let to_px = |p: (f64, f64)| {
        (
            MARGIN + p.0 * (W - 2.0 * MARGIN),
            MARGIN + p.1 * (H - 2.0 * MARGIN),
        )
    };

    let mut s = String::new();
    let _ = writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.0}\" height=\"{:.0}\" viewBox=\"0 0 {:.0} {:.0}\" font-family=\"sans-serif\">",
        W, H, W, H
    );
    let _ = writeln!(
        s,
        "<rect width=\"{:.0}\" height=\"{:.0}\" fill=\"#1a1a2e\"/>",
        W, H
    );

    // Edges.
    s.push_str("<g stroke=\"#aaaaaa\">\n");
    for e in &view.edges {
        let (x0, y0) = to_px(pos[e.source]);
        let (x1, y1) = to_px(pos[e.target]);
        let strong = matches!(e.kind.as_deref(), Some("call") | Some("methodcall"));
        let opacity = if strong { 0.55 } else { 0.3 };
        let dash = if !strong && e.kind.is_some() {
            " stroke-dasharray=\"4 3\""
        } else {
            ""
        };
        let _ = writeln!(
            s,
            "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke-width=\"0.8\" stroke-opacity=\"{}\"{}/>",
            x0, y0, x1, y1, opacity, dash
        );
    }
    s.push_str("</g>\n");

    // Nodes + selective labels.
    for (i, node) in view.nodes.iter().enumerate() {
        let (x, y) = to_px(pos[i]);
        let r = 4.0 + 16.0 * (node.degree as f64 / max_deg);
        let _ = writeln!(
            s,
            "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"{:.1}\" fill=\"{}\" fill-opacity=\"0.9\"/>",
            x,
            y,
            r,
            color_for(node.community)
        );
        if node.degree as f64 >= max_deg * 0.15 {
            let _ = writeln!(
                s,
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"9\" fill=\"#ffffff\" text-anchor=\"middle\">{}</text>",
                x,
                y - r - 2.0,
                escape_xml(&node.label)
            );
        }
    }

    // Legend.
    let _ = writeln!(
        s,
        "<g transform=\"translate(20,20)\"><rect x=\"-8\" y=\"-8\" width=\"260\" height=\"{:.0}\" fill=\"#2a2a4e\" fill-opacity=\"0.7\" rx=\"6\"/>",
        24.0 * (view.communities.len() as f64) + 16.0
    );
    for (i, c) in view.communities.iter().enumerate() {
        let y = 12.0 + 24.0 * i as f64;
        let _ = writeln!(
            s,
            "<circle cx=\"4\" cy=\"{:.0}\" r=\"6\" fill=\"{}\"/><text x=\"18\" y=\"{:.0}\" font-size=\"12\" fill=\"#ffffff\">{} ({})</text>",
            y,
            color_for(c.index),
            y + 4.0,
            escape_xml(&c.name),
            c.size
        );
    }
    s.push_str("</g>\n</svg>\n");
    s
}

// ── Obsidian canvas ───────────────────────────────────────────────────────────

/// Obsidian canvas colour codes, cycled per community.
const CANVAS_COLORS: [&str; 6] = ["1", "2", "3", "4", "5", "6"];

fn render_canvas(view: &GraphView) -> String {
    const CARD_W: i64 = 220;
    const CARD_H: i64 = 60;
    const CARD_GAP: i64 = 20;
    const PAD: i64 = 40;
    const HEADER: i64 = 50;
    const GROUP_GAP: i64 = 80;

    // Members per community, in node order.
    let k = view.communities.len();
    let mut members_by_comm: Vec<Vec<usize>> = vec![Vec::new(); k];
    let mut pos_of: BTreeMap<usize, usize> = BTreeMap::new();
    for (pos, c) in view.communities.iter().enumerate() {
        pos_of.insert(c.index, pos);
    }
    for (idx, node) in view.nodes.iter().enumerate() {
        if let Some(&pos) = pos_of.get(&node.community) {
            members_by_comm[pos].push(idx);
        }
    }

    // Per-group inner grid + box dimensions.
    let cols = (k as f64).sqrt().ceil().max(1.0) as usize;
    let rows = ((k as f64) / cols as f64).ceil().max(1.0) as usize;
    let mut group_dims: Vec<(i64, i64, usize)> = Vec::with_capacity(k); // (w, h, inner_cols)
    for members in &members_by_comm {
        let n = members.len().max(1);
        let inner_cols = (n as f64).sqrt().ceil().max(1.0) as usize;
        let inner_rows = ((n as f64) / inner_cols as f64).ceil().max(1.0) as usize;
        let w = PAD * 2 + inner_cols as i64 * CARD_W + (inner_cols as i64 - 1) * CARD_GAP;
        let h = HEADER + PAD * 2 + inner_rows as i64 * CARD_H + (inner_rows as i64 - 1) * CARD_GAP;
        group_dims.push((w, h, inner_cols));
    }

    // Column widths / row heights for the outer grid.
    let mut col_w = vec![0i64; cols];
    let mut row_h = vec![0i64; rows];
    for (i, &(w, h, _)) in group_dims.iter().enumerate() {
        let (cx, cy) = (i % cols, i / cols);
        col_w[cx] = col_w[cx].max(w);
        row_h[cy] = row_h[cy].max(h);
    }
    let col_x: Vec<i64> = (0..cols)
        .map(|c| col_w[..c].iter().sum::<i64>() + c as i64 * GROUP_GAP)
        .collect();
    let row_y: Vec<i64> = (0..rows)
        .map(|r| row_h[..r].iter().sum::<i64>() + r as i64 * GROUP_GAP)
        .collect();

    let mut nodes_json: Vec<Value> = Vec::new();
    let mut card_id_of: Vec<Option<String>> = vec![None; view.nodes.len()];

    for (gi, members) in members_by_comm.iter().enumerate() {
        let (gw, gh, inner_cols) = group_dims[gi];
        let (cx, cy) = (gi % cols, gi / cols);
        let gx = col_x[cx];
        let gy = row_y[cy];
        let community = view.communities[gi].index;

        nodes_json.push(json!({
            "id": format!("g{}", gi),
            "type": "group",
            "label": community_name(view, community),
            "x": gx, "y": gy, "width": gw, "height": gh,
            "color": CANVAS_COLORS[gi % CANVAS_COLORS.len()],
        }));

        for (mi, &node_idx) in members.iter().enumerate() {
            let ic = mi % inner_cols;
            let ir = mi / inner_cols;
            let x = gx + PAD + ic as i64 * (CARD_W + CARD_GAP);
            let y = gy + HEADER + PAD + ir as i64 * (CARD_H + CARD_GAP);
            let card_id = format!("c{}", node_idx);
            card_id_of[node_idx] = Some(card_id.clone());
            nodes_json.push(json!({
                "id": card_id,
                "type": "text",
                "text": view.nodes[node_idx].label,
                "x": x, "y": y, "width": CARD_W, "height": CARD_H,
                "color": CANVAS_COLORS[gi % CANVAS_COLORS.len()],
            }));
        }
    }

    let edges_json: Vec<Value> = view
        .edges
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let from = card_id_of[e.source].as_ref()?;
            let to = card_id_of[e.target].as_ref()?;
            Some(json!({ "id": format!("e{}", i), "fromNode": from, "toNode": to }))
        })
        .collect();

    serde_json::to_string_pretty(&json!({ "nodes": nodes_json, "edges": edges_json }))
        .unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[]}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CommunityMeta, GraphEdge, GraphLevel, GraphNode, GraphView};

    fn two_community_view() -> GraphView {
        // Two triangles (communities 0 and 1) joined by a single bridge edge.
        let nodes = (0..6)
            .map(|i| GraphNode {
                id: format!("node{}", i),
                label: format!("n{}", i),
                community: if i < 3 { 0 } else { 1 },
                degree: 0,
                language: "rust".to_string(),
                repository: None,
            })
            .collect();
        let mut edges = vec![
            (0, 1),
            (1, 2),
            (0, 2),
            (3, 4),
            (4, 5),
            (3, 5),
            (2, 3), // bridge
        ]
        .into_iter()
        .map(|(s, t)| GraphEdge {
            source: s,
            target: t,
            weight: 1.0,
            kind: Some("call".to_string()),
        })
        .collect::<Vec<_>>();
        edges.sort_by_key(|e| (e.source, e.target));
        GraphView {
            repository_id: "repo".to_string(),
            level: GraphLevel::File,
            nodes,
            edges,
            communities: vec![
                CommunityMeta {
                    index: 0,
                    name: "alpha".into(),
                    size: 3,
                    cohesion: 1.0,
                },
                CommunityMeta {
                    index: 1,
                    name: "beta".into(),
                    size: 3,
                    cohesion: 1.0,
                },
            ],
        }
    }

    #[test]
    fn layout_is_deterministic() {
        let view = two_community_view();
        assert_eq!(layout(&view), layout(&view));
    }

    #[test]
    fn aggregate_collapses_to_meta_graph() {
        let view = two_community_view();
        let agg = aggregate(&view);
        // One node per community, one cross-community (bridge) edge.
        assert_eq!(agg.nodes.len(), 2);
        assert_eq!(agg.edges.len(), 1);
        assert_eq!(agg.edges[0].weight, 1.0);
    }

    #[test]
    fn html_contains_payload_and_palette() {
        let html = render_html(&two_community_view());
        assert!(html.contains("vis-network"));
        assert!(html.contains("RAW_NODES"));
        assert!(html.contains(COMMUNITY_COLORS[0]));
    }

    #[test]
    fn canvas_is_valid_json_with_groups() {
        let canvas = render_canvas(&two_community_view());
        let parsed: Value = serde_json::from_str(&canvas).unwrap();
        let nodes = parsed["nodes"].as_array().unwrap();
        // 2 groups + 6 cards.
        assert_eq!(nodes.len(), 8);
        assert_eq!(parsed["edges"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn svg_has_circles_and_lines() {
        let svg = render_svg(&two_community_view());
        assert!(svg.starts_with("<svg"));
        assert!(svg.matches("<circle").count() >= 6);
        assert_eq!(svg.matches("<line").count(), 7);
    }
}
