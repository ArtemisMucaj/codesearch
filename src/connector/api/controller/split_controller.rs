use anyhow::Result;

use crate::cli::GraphFormat;
use crate::connector::api::container::Container;
use crate::domain::SplitAnalysis;

pub struct SplitController<'a> {
    container: &'a Container,
}

impl<'a> SplitController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// Resolve a user-supplied string to a repository ID.
    ///
    /// Tries an exact match against ID first, then falls back to a
    /// case-insensitive match against repository name.
    async fn resolve_repo_id(&self, name_or_id: &str) -> Result<String> {
        let repos = self
            .container
            .metadata_repository()
            .list()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list repositories: {e}"))?;

        // Exact ID match
        if let Some(r) = repos.iter().find(|r| r.id().to_string() == name_or_id) {
            return Ok(r.id().to_string());
        }

        // Case-insensitive name match
        let lower = name_or_id.to_lowercase();
        if let Some(r) = repos.iter().find(|r| r.name().to_lowercase() == lower) {
            return Ok(r.id().to_string());
        }

        let available = repos
            .iter()
            .map(|r| format!("  {} (id: {})", r.name(), r.id()))
            .collect::<Vec<_>>()
            .join("\n");
        Err(anyhow::anyhow!(
            "Repository '{}' not found.\nAvailable repositories:\n{}",
            name_or_id,
            if available.is_empty() { "  (none indexed yet)".to_string() } else { available }
        ))
    }

    pub async fn split(&self, repository: String, format: GraphFormat) -> Result<String> {
        // Accept either a repository ID or a repository name.
        let target_id = self.resolve_repo_id(&repository).await?;
        let use_case = self.container.split_analysis_use_case();
        let analysis = use_case.analyse(&target_id).await?;

        match format {
            GraphFormat::Html => Ok(format_html(&analysis)),
            GraphFormat::Json => Ok(serde_json::to_string_pretty(&analysis)?),
            GraphFormat::Dot | GraphFormat::Mermaid => {
                Err(anyhow::anyhow!(
                    "Only --format html and --format json are supported for the split command"
                ))
            }
        }
    }
}

// ── HTML renderer ─────────────────────────────────────────────────────────────

fn format_html(analysis: &SplitAnalysis) -> String {
    let data_json = build_data_json(analysis);
    HTML_TEMPLATE.replace("__SPLIT_DATA__", &data_json)
}

/// Serialise the analysis into the compact JSON structure consumed by the
/// client-side Sigma.js visualisation script.
fn build_data_json(analysis: &SplitAnalysis) -> String {
    use std::collections::HashMap;

    // Build a colour palette — one distinct hue per group.
    let palette = [
        "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f",
        "#edc948", "#b07aa1", "#ff9da7", "#9c755f", "#bab0ac",
        "#17becf", "#aec7e8", "#ffbb78", "#98df8a", "#ff9896",
        "#c5b0d5", "#c49c94", "#f7b6d2", "#dbdb8d", "#9edae5",
    ];

    let groups_json: Vec<String> = analysis
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let color = palette[i % palette.len()];
            let public_files = serde_json::to_string(&g.public_files).unwrap_or_default();
            let support_files = serde_json::to_string(&g.support_files).unwrap_or_default();
            let consumers = serde_json::to_string(&g.consumers).unwrap_or_default();
            format!(
                r#"{{"id":{id},"label":{label},"color":{color},"consumers":{consumers},"publicFiles":{public_files},"supportFiles":{support_files}}}"#,
                id = serde_json::to_string(&g.id).unwrap_or_default(),
                label = serde_json::to_string(&g.label).unwrap_or_default(),
                color = serde_json::to_string(color).unwrap_or_default(),
            )
        })
        .collect();

    // Consumer repo objects
    let consumers_json: Vec<String> = analysis
        .consumers
        .values()
        .map(|c| {
            format!(
                r#"{{"id":{id},"name":{name},"path":{path}}}"#,
                id = serde_json::to_string(&c.id).unwrap_or_default(),
                name = serde_json::to_string(&c.name).unwrap_or_default(),
                path = serde_json::to_string(&c.path).unwrap_or_default(),
            )
        })
        .collect();

    // Map file → group index for quick lookup on the client
    let mut file_group: HashMap<&str, usize> = HashMap::new();
    for (i, g) in analysis.groups.iter().enumerate() {
        for f in &g.public_files {
            file_group.insert(f.as_str(), i);
        }
        for f in &g.support_files {
            file_group.insert(f.as_str(), i);
        }
    }

    format!(
        r#"{{"target":{{"id":{target_id},"name":{target_name},"path":{target_path}}},"groups":[{groups}],"consumers":[{consumers}],"stats":{{"totalFiles":{total},"visibleFiles":{visible},"groupCount":{gcount},"consumerCount":{ccount}}}}}"#,
        target_id = serde_json::to_string(&analysis.target_repo_id).unwrap_or_default(),
        target_name = serde_json::to_string(&analysis.target_repo_name).unwrap_or_default(),
        target_path = serde_json::to_string(&analysis.target_repo_path).unwrap_or_default(),
        groups = groups_json.join(","),
        consumers = consumers_json.join(","),
        total = analysis.total_files_in_target,
        visible = analysis.externally_visible_count,
        gcount = analysis.groups.len(),
        ccount = analysis.consumers.len(),
    )
}

// ── HTML template ─────────────────────────────────────────────────────────────

const HTML_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Monolith Split Analysis</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: #0d1117; color: #c9d1d9; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", monospace; display: flex; height: 100vh; overflow: hidden; }

  /* Sidebar */
  #sidebar { width: 320px; min-width: 220px; background: #161b22; border-right: 1px solid #30363d; display: flex; flex-direction: column; overflow: hidden; }
  #sidebar-header { padding: 16px; border-bottom: 1px solid #30363d; }
  #sidebar-header h1 { font-size: 14px; font-weight: 600; color: #e6edf3; }
  #sidebar-header .target-name { font-size: 18px; font-weight: 700; color: #58a6ff; margin-top: 4px; word-break: break-all; }
  #sidebar-header .target-path { font-size: 11px; color: #8b949e; margin-top: 2px; word-break: break-all; }

  #stats-bar { display: flex; gap: 0; border-bottom: 1px solid #30363d; }
  .stat-cell { flex: 1; padding: 10px 8px; text-align: center; border-right: 1px solid #30363d; }
  .stat-cell:last-child { border-right: none; }
  .stat-value { font-size: 20px; font-weight: 700; color: #58a6ff; }
  .stat-label { font-size: 9px; text-transform: uppercase; letter-spacing: 0.05em; color: #8b949e; margin-top: 2px; }

  #groups-panel { flex: 1; overflow-y: auto; padding: 8px 0; }
  .group-item { padding: 10px 14px; cursor: pointer; border-left: 3px solid transparent; transition: background 0.15s; }
  .group-item:hover { background: #21262d; }
  .group-item.active { background: #21262d; }
  .group-dot { display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 8px; flex-shrink: 0; vertical-align: middle; }
  .group-label { font-size: 12px; font-weight: 600; color: #e6edf3; vertical-align: middle; }
  .group-meta { font-size: 11px; color: #8b949e; margin-top: 4px; padding-left: 18px; }
  .group-consumers { font-size: 10px; color: #8b949e; margin-top: 2px; padding-left: 18px; font-style: italic; }

  #consumers-section { border-top: 1px solid #30363d; padding: 10px 14px; }
  #consumers-section h3 { font-size: 11px; text-transform: uppercase; letter-spacing: 0.08em; color: #8b949e; margin-bottom: 8px; }
  .consumer-item { display: flex; align-items: center; gap: 8px; padding: 4px 0; font-size: 11px; cursor: pointer; border-radius: 4px; padding: 4px 6px; }
  .consumer-item:hover { background: #21262d; }
  .consumer-dot { width: 8px; height: 8px; border-radius: 50%; background: #e36209; flex-shrink: 0; }
  .consumer-name { color: #e6edf3; font-weight: 500; }
  .consumer-path { color: #8b949e; font-size: 10px; margin-left: auto; }

  #hint { padding: 8px 14px; font-size: 10px; color: #6e7681; border-top: 1px solid #30363d; line-height: 1.5; }

  /* Graph area */
  #graph-area { flex: 1; position: relative; background: #0d1117; }
  #sigma-container { width: 100%; height: 100%; }

  /* Tooltip */
  #tooltip { position: absolute; background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 10px 14px; font-size: 12px; pointer-events: none; max-width: 320px; z-index: 100; display: none; box-shadow: 0 4px 16px rgba(0,0,0,0.5); }
  #tooltip .tt-title { font-weight: 600; color: #e6edf3; margin-bottom: 4px; word-break: break-all; }
  #tooltip .tt-meta { color: #8b949e; font-size: 11px; line-height: 1.6; }
  #tooltip .tt-tag { display: inline-block; background: #21262d; border-radius: 3px; padding: 1px 6px; font-size: 10px; margin: 2px 2px 0 0; }

  /* Search */
  #search-box { position: absolute; top: 12px; right: 12px; z-index: 10; }
  #search-box input { background: #161b22; border: 1px solid #30363d; border-radius: 6px; color: #c9d1d9; padding: 6px 10px; font-size: 12px; width: 220px; outline: none; }
  #search-box input:focus { border-color: #58a6ff; }
  #search-box input::placeholder { color: #6e7681; }

  /* Legend */
  #legend { position: absolute; bottom: 12px; right: 12px; background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 10px 14px; font-size: 11px; }
  .legend-row { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
  .legend-row:last-child { margin-bottom: 0; }
  .legend-circle { border-radius: 50%; flex-shrink: 0; }
  .legend-text { color: #8b949e; }

  /* No-data banner */
  #no-data { display: none; position: absolute; top: 50%; left: 50%; transform: translate(-50%,-50%); text-align: center; color: #8b949e; }
  #no-data h2 { font-size: 18px; color: #e6edf3; margin-bottom: 8px; }
</style>
</head>
<body>

<div id="sidebar">
  <div id="sidebar-header">
    <div class="label" style="font-size:11px;text-transform:uppercase;letter-spacing:.08em;color:#8b949e">Monolith Split Analysis</div>
    <div class="target-name" id="target-name">—</div>
    <div class="target-path" id="target-path"></div>
  </div>
  <div id="stats-bar">
    <div class="stat-cell"><div class="stat-value" id="stat-total">—</div><div class="stat-label">Total files</div></div>
    <div class="stat-cell"><div class="stat-value" id="stat-visible">—</div><div class="stat-label">Ext. visible</div></div>
    <div class="stat-cell"><div class="stat-value" id="stat-groups">—</div><div class="stat-label">Groups</div></div>
    <div class="stat-cell"><div class="stat-value" id="stat-consumers">—</div><div class="stat-label">Consumers</div></div>
  </div>
  <div id="groups-panel"></div>
  <div id="consumers-section">
    <h3>External Consumers</h3>
    <div id="consumers-list"></div>
  </div>
  <div id="hint">
    Click a group or consumer to highlight connections.<br>
    Scroll to zoom · Drag to pan · Shift+drag to select.
  </div>
</div>

<div id="graph-area">
  <div id="sigma-container"></div>
  <div id="search-box"><input type="text" id="search-input" placeholder="Search files…"></div>
  <div id="legend">
    <div class="legend-row"><div class="legend-circle" style="width:18px;height:18px;background:#2ea04326;border:2px solid #2ea043"></div><span class="legend-text">Extraction candidate group</span></div>
    <div class="legend-row"><div class="legend-circle" style="width:10px;height:10px;background:#58a6ff"></div><span class="legend-text">Public interface file</span></div>
    <div class="legend-row"><div class="legend-circle" style="width:8px;height:8px;background:#58a6ff;opacity:.5"></div><span class="legend-text">Internal support file</span></div>
    <div class="legend-row"><div class="legend-circle" style="width:12px;height:12px;background:#e36209"></div><span class="legend-text">Consumer repository</span></div>
  </div>
  <div id="tooltip"><div class="tt-title" id="tt-title"></div><div class="tt-meta" id="tt-meta"></div></div>
  <div id="no-data"><h2>No cross-repository dependencies found</h2><p>No external repositories reference this monolith's files.</p></div>
</div>

<!-- Sigma.js v2 + graphology from CDN (sigma@3 not yet stable on CDN) -->
<script src="https://cdn.jsdelivr.net/npm/graphology@0.25.4/dist/graphology.umd.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/sigma@2.4.0/build/sigma.min.js"></script>

<script>
// ── Utilities (declared first so sidebar code can call escHtml) ───────────────
function basename(path) { return path.split('/').pop() || path; }
function shortPath(path) { const p = path.split('/'); return p.length > 2 ? '…/' + p.slice(-2).join('/') : path; }
function escHtml(s) { return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }
function hexRgb(hex) { return [parseInt(hex.slice(1,3),16), parseInt(hex.slice(3,5),16), parseInt(hex.slice(5,7),16)]; }
function hexWithAlpha(hex, alpha) { const [r,g,b] = hexRgb(hex); return `rgba(${r},${g},${b},${alpha})`; }
function dimColor(color) {
  if (!color) return '#2d333b';
  if (color.startsWith('rgba') || color.startsWith('rgb')) return 'rgba(50,60,70,0.25)';
  try { const [r,g,b] = hexRgb(color); return `rgba(${r},${g},${b},0.15)`; } catch(e) { return '#2d333b'; }
}

// ── State (declared before any event handler registration) ────────────────────
var activeGroupId = null;
var activeConsumerId = null;

// ── Data ──────────────────────────────────────────────────────────────────────
const DATA = __SPLIT_DATA__;

// ── Populate sidebar ─────────────────────────────────────────────────────────
document.getElementById('target-name').textContent = DATA.target.name;
document.getElementById('target-path').textContent = DATA.target.path;
document.getElementById('stat-total').textContent = DATA.stats.totalFiles;
document.getElementById('stat-visible').textContent = DATA.stats.visibleFiles;
document.getElementById('stat-groups').textContent = DATA.stats.groupCount;
document.getElementById('stat-consumers').textContent = DATA.stats.consumerCount;

if (DATA.stats.groupCount === 0) {
  document.getElementById('no-data').style.display = 'block';
}

const groupsPanel = document.getElementById('groups-panel');
DATA.groups.forEach((g) => {
  const div = document.createElement('div');
  div.className = 'group-item';
  div.dataset.groupId = g.id;
  const totalFiles = g.publicFiles.length + g.supportFiles.length;
  div.innerHTML =
    '<div><span class="group-dot" style="background:' + g.color + '"></span>' +
    '<span class="group-label">' + escHtml(g.label) + '</span></div>' +
    '<div class="group-meta">' + g.publicFiles.length + ' public · ' + g.supportFiles.length + ' support · ' + totalFiles + ' total</div>' +
    '<div class="group-consumers">Used by: ' + escHtml(g.consumers.join(', ')) + '</div>';
  div.addEventListener('click', function() { selectGroup(g.id); });
  groupsPanel.appendChild(div);
});

const consumersList = document.getElementById('consumers-list');
DATA.consumers.forEach(function(c) {
  const div = document.createElement('div');
  div.className = 'consumer-item';
  div.innerHTML = '<div class="consumer-dot"></div><span class="consumer-name">' + escHtml(c.name) +
    '</span><span class="consumer-path" title="' + escHtml(c.path) + '">' + escHtml(shortPath(c.path)) + '</span>';
  div.addEventListener('click', function() { selectConsumer(c.id); });
  consumersList.appendChild(div);
});

// ── Build graph ──────────────────────────────────────────────────────────────
const graph = new graphology.Graph({ multi: false, type: 'directed' });

const CX = 0, CY = 0;
const MONOLITH_R = 600;
const GROUP_INNER_R = 120;
const CONSUMER_R = 950;

// Place group centers on a ring inside the monolith
const groupCenters = {};
DATA.groups.forEach(function(g, i) {
  const angle = (2 * Math.PI * i) / Math.max(DATA.groups.length, 1) - Math.PI / 2;
  const dist = DATA.groups.length <= 1 ? 0 : MONOLITH_R * 0.52;
  groupCenters[g.id] = { x: CX + dist * Math.cos(angle), y: CY + dist * Math.sin(angle) };
});

// Group centroid nodes
DATA.groups.forEach(function(g, i) {
  const c = groupCenters[g.id];
  graph.addNode('group::' + g.id, {
    x: c.x, y: c.y,
    size: Math.max(14, 5 + (g.publicFiles.length + g.supportFiles.length) * 0.8),
    color: g.color,
    label: g.label,
    nodeType: 'group',
    groupId: g.id,
    groupColor: g.color,
    groupIdx: i,
  });
});

// File nodes
DATA.groups.forEach(function(g) {
  const center = groupCenters[g.id];
  const allFiles = g.publicFiles.map(function(f) { return { f: f, isSupport: false }; })
    .concat(g.supportFiles.map(function(f) { return { f: f, isSupport: true }; }));
  allFiles.forEach(function(item, j) {
    const angle = (2 * Math.PI * j) / Math.max(allFiles.length, 1);
    const r = allFiles.length <= 1 ? 0 : GROUP_INNER_R * 0.75;
    const nodeKey = 'file::' + item.f;
    if (!graph.hasNode(nodeKey)) {
      graph.addNode(nodeKey, {
        x: center.x + r * Math.cos(angle),
        y: center.y + r * Math.sin(angle),
        size: item.isSupport ? 4 : 6,
        color: item.isSupport ? hexWithAlpha(g.color, 0.5) : g.color,
        label: basename(item.f),
        fullPath: item.f,
        nodeType: item.isSupport ? 'support' : 'file',
        groupId: g.id,
        groupColor: g.color,
      });
    }
  });
});

// Consumer nodes in outer ring
DATA.consumers.forEach(function(c, i) {
  const angle = (2 * Math.PI * i) / Math.max(DATA.consumers.length, 1) - Math.PI / 2;
  graph.addNode('consumer::' + c.id, {
    x: CX + CONSUMER_R * Math.cos(angle),
    y: CY + CONSUMER_R * Math.sin(angle),
    size: 16,
    color: '#e36209',
    label: c.name,
    nodeType: 'consumer',
    consumerId: c.id,
  });
});

// Edges: group centroid → consumer
DATA.groups.forEach(function(g) {
  g.consumers.forEach(function(cid) {
    const src = 'group::' + g.id;
    const tgt = 'consumer::' + cid;
    if (graph.hasNode(src) && graph.hasNode(tgt)) {
      const ekey = src + '-->' + tgt;
      if (!graph.hasEdge(ekey)) {
        graph.addEdgeWithKey(ekey, src, tgt, { size: 1.5, color: '#444c56' });
      }
    }
  });
});

// ── Sigma v2 renderer ─────────────────────────────────────────────────────────
const sigmaContainer = document.getElementById('sigma-container');

const renderer = new Sigma(graph, sigmaContainer, {
  renderEdgeLabels: false,
  defaultEdgeColor: '#444c56',
  defaultNodeColor: '#58a6ff',
  labelColor: { color: '#c9d1d9' },
  labelSize: 11,
  labelWeight: 'normal',
  minCameraRatio: 0.04,
  maxCameraRatio: 12,
  labelRenderedSizeThreshold: 5,
});

// ── SVG overlay for group halos ───────────────────────────────────────────────
const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
svg.style.cssText = 'position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none;z-index:1;';
sigmaContainer.style.position = 'relative';
sigmaContainer.appendChild(svg);

function updateHalos() {
  svg.innerHTML = '';

  // Monolith boundary — project two points to get pixel radius
  const mCenter = renderer.graphToViewport({ x: CX, y: CY });
  const mEdge   = renderer.graphToViewport({ x: CX + MONOLITH_R, y: CY });
  const mR = Math.max(40, Math.abs(mEdge.x - mCenter.x));

  const mc = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
  mc.setAttribute('cx', mCenter.x); mc.setAttribute('cy', mCenter.y); mc.setAttribute('r', mR);
  mc.setAttribute('fill', 'none'); mc.setAttribute('stroke', '#388bfd');
  mc.setAttribute('stroke-width', '2'); mc.setAttribute('stroke-dasharray', '8 4');
  mc.setAttribute('opacity', '0.3');
  svg.appendChild(mc);

  const mlabel = document.createElementNS('http://www.w3.org/2000/svg', 'text');
  mlabel.setAttribute('x', mCenter.x); mlabel.setAttribute('y', mCenter.y - mR + 16);
  mlabel.setAttribute('text-anchor', 'middle'); mlabel.setAttribute('fill', '#58a6ff');
  mlabel.setAttribute('font-size', '13'); mlabel.setAttribute('font-family', 'monospace');
  mlabel.setAttribute('opacity', '0.65');
  mlabel.textContent = DATA.target.name;
  svg.appendChild(mlabel);

  // Group halos
  DATA.groups.forEach(function(g) {
    const center = groupCenters[g.id];
    const cVp  = renderer.graphToViewport({ x: center.x, y: center.y });
    const eVp  = renderer.graphToViewport({ x: center.x + GROUP_INNER_R, y: center.y });
    const haloR = Math.max(20, Math.abs(eVp.x - cVp.x) + 12);

    const circle = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
    circle.setAttribute('cx', cVp.x); circle.setAttribute('cy', cVp.y); circle.setAttribute('r', haloR);
    circle.setAttribute('fill', hexWithAlpha(g.color, 0.07));
    circle.setAttribute('stroke', g.color); circle.setAttribute('stroke-width', '1.5');
    circle.setAttribute('stroke-dasharray', '4 3'); circle.setAttribute('opacity', '0.75');
    svg.appendChild(circle);
  });
}

renderer.on('afterRender', updateHalos);

// ── Tooltip ───────────────────────────────────────────────────────────────────
const tooltip  = document.getElementById('tooltip');
const ttTitle  = document.getElementById('tt-title');
const ttMeta   = document.getElementById('tt-meta');

renderer.on('enterNode', function(e) {
  const attrs = graph.getNodeAttributes(e.node);
  ttTitle.textContent = attrs.label || e.node;
  var meta = '';
  if (attrs.nodeType === 'group') {
    const g = DATA.groups[attrs.groupIdx];
    meta = 'Extraction candidate group<br>' + g.publicFiles.length + ' public · ' + g.supportFiles.length + ' support files<br>Consumers: ' + escHtml(g.consumers.join(', '));
  } else if (attrs.nodeType === 'file' || attrs.nodeType === 'support') {
    meta = (attrs.nodeType === 'support' ? 'Internal support file' : 'Public interface file') +
      '<br><span class="tt-tag">group: ' + escHtml(attrs.groupId) + '</span>';
    if (attrs.fullPath) meta += '<br><span style="color:#8b949e;font-size:10px">' + escHtml(attrs.fullPath) + '</span>';
  } else if (attrs.nodeType === 'consumer') {
    const c = DATA.consumers.find(function(x) { return x.id === attrs.consumerId; });
    meta = 'External consumer<br>' + (c ? escHtml(c.path) : '');
  }
  ttMeta.innerHTML = meta;
  tooltip.style.left = (e.event.x + 16) + 'px';
  tooltip.style.top  = (e.event.y + 16) + 'px';
  tooltip.style.display = 'block';
});

renderer.on('leaveNode', function() { tooltip.style.display = 'none'; });

// ── Selection / highlight ─────────────────────────────────────────────────────
function clearSelection() {
  activeGroupId = null;
  activeConsumerId = null;
  document.querySelectorAll('.group-item').forEach(function(el) { el.classList.remove('active'); });
  graph.forEachNode(function(n, attrs) {
    graph.setNodeAttribute(n, 'color', attrs.origColor || attrs.color);
  });
  graph.forEachEdge(function(e) {
    graph.setEdgeAttribute(e, 'color', '#444c56');
    graph.setEdgeAttribute(e, 'size', 1.5);
  });
}

// Store original colours once on first use
function storeOrigColors() {
  graph.forEachNode(function(n, attrs) {
    if (!attrs.origColor) graph.setNodeAttribute(n, 'origColor', attrs.color);
  });
}

function selectGroup(gid) {
  storeOrigColors();
  if (activeGroupId === gid) { clearSelection(); return; }
  clearSelection();
  activeGroupId = gid;
  document.querySelectorAll('.group-item').forEach(function(el) {
    if (el.dataset.groupId === gid) el.classList.add('active');
  });
  const g = DATA.groups.find(function(x) { return x.id === gid; });
  if (!g) return;
  const rel = new Set();
  rel.add('group::' + gid);
  g.publicFiles.forEach(function(f) { rel.add('file::' + f); });
  g.supportFiles.forEach(function(f) { rel.add('file::' + f); });
  g.consumers.forEach(function(c) { rel.add('consumer::' + c); });
  graph.forEachNode(function(n, attrs) {
    if (!rel.has(n)) graph.setNodeAttribute(n, 'color', dimColor(attrs.origColor || attrs.color));
  });
  graph.forEachEdge(function(e, attrs, src, tgt) {
    if (rel.has(src) && rel.has(tgt)) { graph.setEdgeAttribute(e, 'color', g.color); graph.setEdgeAttribute(e, 'size', 2.5); }
    else graph.setEdgeAttribute(e, 'color', '#2d333b');
  });
}

function selectConsumer(cid) {
  storeOrigColors();
  if (activeConsumerId === cid) { clearSelection(); return; }
  clearSelection();
  activeConsumerId = cid;
  const rel = new Set(['consumer::' + cid]);
  DATA.groups.forEach(function(g) {
    if (g.consumers.includes(cid)) {
      rel.add('group::' + g.id);
      g.publicFiles.forEach(function(f) { rel.add('file::' + f); });
    }
  });
  graph.forEachNode(function(n, attrs) {
    if (!rel.has(n)) graph.setNodeAttribute(n, 'color', dimColor(attrs.origColor || attrs.color));
  });
  graph.forEachEdge(function(e, attrs, src, tgt) {
    if (rel.has(src) && rel.has(tgt)) { graph.setEdgeAttribute(e, 'color', '#e36209'); graph.setEdgeAttribute(e, 'size', 2.5); }
    else graph.setEdgeAttribute(e, 'color', '#2d333b');
  });
}

renderer.on('clickStage', clearSelection);
renderer.on('clickNode', function(e) {
  const attrs = graph.getNodeAttributes(e.node);
  if (attrs.nodeType === 'group' || attrs.nodeType === 'file' || attrs.nodeType === 'support') selectGroup(attrs.groupId);
  else if (attrs.nodeType === 'consumer') selectConsumer(attrs.consumerId);
});

// ── Search ─────────────────────────────────────────────────────────────────────
document.getElementById('search-input').addEventListener('input', function(e) {
  const q = e.target.value.trim().toLowerCase();
  if (!q) { clearSelection(); return; }
  storeOrigColors();
  const matched = new Set();
  graph.forEachNode(function(n, attrs) {
    if ((attrs.label || '').toLowerCase().includes(q) || (attrs.fullPath || '').toLowerCase().includes(q)) matched.add(n);
  });
  graph.forEachNode(function(n, attrs) {
    if (!matched.has(n)) graph.setNodeAttribute(n, 'color', dimColor(attrs.origColor || attrs.color));
  });
});
</script>
</body>
</html>
"#;
