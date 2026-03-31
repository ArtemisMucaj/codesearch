use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;

use crate::cli::{ClusterMode, GraphFormat, NodeGranularity};
use crate::domain::{FileEdge, FileGraph};

use super::super::Container;

pub struct FileGraphController<'a> {
    container: &'a Container,
}

impl<'a> FileGraphController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn graph(
        &self,
        repository: Option<Vec<String>>,
        format: GraphFormat,
        granularity: NodeGranularity,
        min_weight: usize,
        cluster: ClusterMode,
    ) -> Result<String> {
        let use_case = self.container.file_graph_use_case();
        let graph = use_case
            .build_graph(repository.as_deref(), min_weight, true)
            .await?;

        if graph.is_empty() {
            return Ok(
                "No file dependency edges found. Make sure at least one repository is \
                 indexed (codesearch index <path>) and has resolvable symbol references.\n\
                 Tip: try --granularity directory to aggregate files into directories first."
                    .to_string(),
            );
        }

        // Optionally collapse file-level nodes to directory-level.
        let graph = match granularity {
            NodeGranularity::File => graph,
            NodeGranularity::Directory => aggregate_to_directories(graph),
        };

        Ok(match format {
            GraphFormat::Html => Self::format_html(&graph, cluster),
            GraphFormat::Dot => Self::format_dot(&graph, cluster),
            GraphFormat::Mermaid => Self::format_mermaid(&graph, cluster),
            GraphFormat::Json => serde_json::to_string_pretty(&graph)?,
        })
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    fn file_to_repo<'g>(graph: &'g FileGraph) -> HashMap<&'g str, &'g str> {
        let mut map: HashMap<&str, &str> = HashMap::new();
        for edge in &graph.edges {
            map.entry(edge.from_file.as_str())
                .or_insert(edge.from_repo_id.as_str());
            map.entry(edge.to_file.as_str())
                .or_insert(edge.to_repo_id.as_str());
        }
        map
    }

    fn files_by_repo<'g>(graph: &'g FileGraph) -> BTreeMap<&'g str, BTreeSet<&'g str>> {
        let file_repo = Self::file_to_repo(graph);
        let mut map: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (file, repo) in &file_repo {
            map.entry(repo).or_default().insert(file);
        }
        map
    }

    /// For each file, return the set of *other* repo_ids whose edges point to it.
    fn consumer_map<'g>(graph: &'g FileGraph) -> HashMap<&'g str, BTreeSet<&'g str>> {
        let mut map: HashMap<&str, BTreeSet<&str>> = HashMap::new();
        for edge in &graph.edges {
            if edge.from_repo_id != edge.to_repo_id {
                map.entry(edge.to_file.as_str())
                    .or_default()
                    .insert(edge.from_repo_id.as_str());
            }
        }
        map
    }

    // ── Interactive HTML renderer (Sigma.js) ─────────────────────────────

    fn format_html(graph: &FileGraph, _cluster: ClusterMode) -> String {
        let file_repo = Self::file_to_repo(graph);

        // Flat file list — layout computed client-side
        let files_json: Vec<String> = {
            let mut sorted: Vec<&str> = graph.files.iter().map(String::as_str).collect();
            sorted.sort();
            sorted
                .into_iter()
                .map(|f| {
                    let repo_id = file_repo.get(f).copied().unwrap_or("");
                    format!(
                        r#"{{"id":{},"label":{},"repoId":{}}}"#,
                        serde_json::to_string(f).unwrap_or_default(),
                        serde_json::to_string(&short_path(f)).unwrap_or_default(),
                        serde_json::to_string(repo_id).unwrap_or_default(),
                    )
                })
                .collect()
        };

        let edges_json: Vec<String> = graph
            .edges
            .iter()
            .map(|e| {
                format!(
                    r#"{{"source":{},"target":{},"weight":{},"kinds":{}}}"#,
                    serde_json::to_string(&e.from_file).unwrap_or_default(),
                    serde_json::to_string(&e.to_file).unwrap_or_default(),
                    e.weight,
                    serde_json::to_string(&e.reference_kinds.join(", ")).unwrap_or_default(),
                )
            })
            .collect();

        let repos_str = serde_json::to_string(&graph.repositories).unwrap_or_default();

        HTML_TEMPLATE
            .replace("__FILES__", &format!("[{}]", files_json.join(",")))
            .replace("__EDGES__", &format!("[{}]", edges_json.join(",")))
            .replace("__REPOS__", &repos_str)
    }

    // ── DOT renderer ──────────────────────────────────────────────────────

    fn format_dot(graph: &FileGraph, cluster: ClusterMode) -> String {
        let mut out = String::new();
        out.push_str(
            "digraph file_dependencies {\n\
             \trankdir=LR;\n\
             \tnode [shape=box fontname=\"Helvetica\" fontsize=10];\n\
             \tedge [fontsize=9];\n\n",
        );

        let repo_files = Self::files_by_repo(graph);

        for (repo_idx, (repo_id, files)) in repo_files.iter().enumerate() {
            let repo_name = graph
                .repositories
                .get(*repo_id)
                .map(|r| r.name.as_str())
                .unwrap_or(repo_id);

            out.push_str(&format!(
                "\tsubgraph cluster_r{idx} {{\n\
                 \t\tlabel=\"{label}\";\n\
                 \t\tstyle=filled;\n\
                 \t\tcolor=lightblue;\n\
                 \t\tfontname=\"Helvetica Bold\";\n\
                 \t\tfontsize=12;\n\n",
                idx = repo_idx,
                label = dot_escape(repo_name),
            ));

            match cluster {
                ClusterMode::None => {
                    for file in files {
                        dot_emit_node(&mut out, file, "\t\t");
                    }
                }
                ClusterMode::Directory => {
                    Self::dot_emit_dir_subclusters(&mut out, repo_idx, files);
                }
                ClusterMode::Consumer => {
                    let consumers = Self::consumer_map(graph);
                    Self::dot_emit_consumer_subclusters(
                        &mut out, repo_idx, repo_id, files, &consumers, graph,
                    );
                }
            }

            out.push_str("\t}\n\n");
        }

        for edge in &graph.edges {
            let attrs = if edge.weight > 1 {
                format!(" [label=\"{}\"]", edge.weight)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "\t{} -> {}{};\n",
                dot_node_id(&edge.from_file),
                dot_node_id(&edge.to_file),
                attrs,
            ));
        }

        out.push_str("}\n");
        out
    }

    fn dot_emit_dir_subclusters(out: &mut String, repo_idx: usize, files: &BTreeSet<&str>) {
        let mut by_dir: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for file in files {
            by_dir.entry(file_dir(file)).or_default().push(file);
        }
        for (dir_idx, (dir, dir_files)) in by_dir.iter().enumerate() {
            let label = if dir.is_empty() { "/" } else { dir };
            out.push_str(&format!(
                "\t\tsubgraph cluster_r{ri}_d{di} {{\n\
                 \t\t\tlabel=\"{label}\";\n\
                 \t\t\tstyle=filled;\n\
                 \t\t\tcolor=lightyellow;\n\
                 \t\t\tfontsize=10;\n\n",
                ri = repo_idx,
                di = dir_idx,
                label = dot_escape(label),
            ));
            for file in dir_files {
                dot_emit_node(out, file, "\t\t\t");
            }
            out.push_str("\t\t}\n");
        }
    }

    fn dot_emit_consumer_subclusters(
        out: &mut String,
        repo_idx: usize,
        _repo_id: &str,
        files: &BTreeSet<&str>,
        consumers: &HashMap<&str, BTreeSet<&str>>,
        graph: &FileGraph,
    ) {
        let mut by_consumers: BTreeMap<Vec<&str>, Vec<&str>> = BTreeMap::new();
        for file in files {
            let key: Vec<&str> = consumers
                .get(file)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            by_consumers.entry(key).or_default().push(file);
        }
        for (grp_idx, (consumer_ids, grp_files)) in by_consumers.iter().enumerate() {
            let label = if consumer_ids.is_empty() {
                "internal".to_string()
            } else {
                let names: Vec<&str> = consumer_ids
                    .iter()
                    .map(|id| {
                        graph
                            .repositories
                            .get(*id)
                            .map(|r| r.name.as_str())
                            .unwrap_or(id)
                    })
                    .collect();
                format!("← {}", names.join(", "))
            };
            out.push_str(&format!(
                "\t\tsubgraph cluster_r{ri}_c{ci} {{\n\
                 \t\t\tlabel=\"{label}\";\n\
                 \t\t\tstyle=filled;\n\
                 \t\t\tcolor={color};\n\
                 \t\t\tfontsize=10;\n\n",
                ri = repo_idx,
                ci = grp_idx,
                label = dot_escape(&label),
                color = if consumer_ids.is_empty() { "white" } else { "lightgreen" },
            ));
            for file in grp_files {
                dot_emit_node(out, file, "\t\t\t");
            }
            out.push_str("\t\t}\n");
        }
    }

    // ── Mermaid renderer ──────────────────────────────────────────────────

    fn format_mermaid(graph: &FileGraph, cluster: ClusterMode) -> String {
        let mut out = String::from("graph LR\n");
        let repo_files = Self::files_by_repo(graph);
        let consumers = Self::consumer_map(graph);

        for (repo_id, files) in &repo_files {
            let repo_name = graph
                .repositories
                .get(*repo_id)
                .map(|r| r.name.as_str())
                .unwrap_or(repo_id);
            out.push_str(&format!(
                "  subgraph {id}[\"{label}\"]\n",
                id = mermaid_id(repo_id),
                label = mermaid_escape(repo_name),
            ));
            match cluster {
                ClusterMode::None => {
                    for file in files {
                        mermaid_emit_node(&mut out, file, "    ");
                    }
                }
                ClusterMode::Directory => {
                    let mut by_dir: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                    for file in files {
                        by_dir.entry(file_dir(file)).or_default().push(file);
                    }
                    for (dir, dir_files) in &by_dir {
                        let label = if dir.is_empty() { "/" } else { dir };
                        let id = format!("{}_{}", mermaid_id(repo_id), mermaid_id(label));
                        out.push_str(&format!(
                            "    subgraph {id}[\"{label}\"]\n",
                            id = id,
                            label = mermaid_escape(label),
                        ));
                        for file in dir_files {
                            mermaid_emit_node(&mut out, file, "      ");
                        }
                        out.push_str("    end\n");
                    }
                }
                ClusterMode::Consumer => {
                    let mut by_consumers: BTreeMap<Vec<&str>, Vec<&str>> = BTreeMap::new();
                    for file in files {
                        let key: Vec<&str> = consumers
                            .get(file)
                            .map(|s| s.iter().copied().collect())
                            .unwrap_or_default();
                        by_consumers.entry(key).or_default().push(file);
                    }
                    for (grp_idx, (consumer_ids, grp_files)) in by_consumers.iter().enumerate() {
                        let label = if consumer_ids.is_empty() {
                            "internal".to_string()
                        } else {
                            let names: Vec<&str> = consumer_ids
                                .iter()
                                .map(|id| {
                                    graph
                                        .repositories
                                        .get(*id)
                                        .map(|r| r.name.as_str())
                                        .unwrap_or(id)
                                })
                                .collect();
                            format!("used by {}", names.join(", "))
                        };
                        let id = format!("{}_c{}", mermaid_id(repo_id), grp_idx);
                        out.push_str(&format!(
                            "    subgraph {id}[\"{label}\"]\n",
                            id = id,
                            label = mermaid_escape(&label),
                        ));
                        for file in grp_files {
                            mermaid_emit_node(&mut out, file, "      ");
                        }
                        out.push_str("    end\n");
                    }
                }
            }
            out.push_str("  end\n");
        }

        out.push('\n');
        for edge in &graph.edges {
            let arrow = if edge.weight > 1 {
                format!(" -- {} --> ", edge.weight)
            } else {
                " --> ".to_string()
            };
            out.push_str(&format!(
                "  {}{}{}\n",
                mermaid_id(&edge.from_file),
                arrow,
                mermaid_id(&edge.to_file),
            ));
        }
        out
    }
}

// ── Directory aggregation ─────────────────────────────────────────────────────

/// Collapse file-level `FileGraph` to directory-level by replacing each
/// `file_path` with its parent directory and re-summing edge weights.
fn aggregate_to_directories(graph: FileGraph) -> FileGraph {
    let mut edge_map: HashMap<(String, String, String, String), (usize, HashSet<String>)> =
        HashMap::new();

    for edge in &graph.edges {
        let from = file_dir(&edge.from_file).to_string();
        let to = file_dir(&edge.to_file).to_string();
        // Skip self-loops that appear after collapsing (files in same directory).
        if from == to && edge.from_repo_id == edge.to_repo_id {
            continue;
        }
        let key = (
            from,
            edge.from_repo_id.clone(),
            to,
            edge.to_repo_id.clone(),
        );
        let entry = edge_map.entry(key).or_insert((0, HashSet::new()));
        entry.0 += edge.weight;
        entry.1.extend(edge.reference_kinds.iter().cloned());
    }

    let mut edges: Vec<FileEdge> = edge_map
        .into_iter()
        .map(
            |((from_file, from_repo_id, to_file, to_repo_id), (weight, kinds))| FileEdge {
                from_file,
                from_repo_id,
                to_file,
                to_repo_id,
                weight,
                reference_kinds: {
                    let mut v: Vec<String> = kinds.into_iter().collect();
                    v.sort();
                    v
                },
            },
        )
        .collect();
    edges.sort_by(|a, b| {
        b.weight
            .cmp(&a.weight)
            .then(a.from_file.cmp(&b.from_file))
            .then(a.to_file.cmp(&b.to_file))
    });

    let files: HashSet<String> = edges
        .iter()
        .flat_map(|e| [e.from_file.clone(), e.to_file.clone()])
        .collect();

    FileGraph {
        repositories: graph.repositories,
        files,
        edges,
    }
}

// ── Rendering helpers ─────────────────────────────────────────────────────────

fn file_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(pos) => &path[..pos],
        None => "",
    }
}

fn short_path(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut slash_count = 0;
    let mut last_pos = None;
    for (i, &b) in bytes.iter().enumerate().rev() {
        if b == b'/' {
            slash_count += 1;
            if slash_count == 2 {
                last_pos = Some(i + 1);
                break;
            }
        }
    }
    last_pos.map(|p| &path[p..]).unwrap_or(path)
}

fn dot_node_id(path: &str) -> String {
    let s: String = path
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("n_{s}")
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn dot_emit_node(out: &mut String, file: &str, indent: &str) {
    out.push_str(&format!(
        "{indent}{id} [label=\"{label}\"];\n",
        id = dot_node_id(file),
        label = dot_escape(short_path(file)),
    ));
}

fn mermaid_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

fn mermaid_escape(s: &str) -> String {
    s.replace('"', "'")
}

fn mermaid_emit_node(out: &mut String, file: &str, indent: &str) {
    out.push_str(&format!(
        "{indent}{id}[\"{label}\"]\n",
        id = mermaid_id(file),
        label = mermaid_escape(short_path(file)),
    ));
}

// ── HTML template (Sigma.js v2) ───────────────────────────────────────────────

const HTML_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>File Dependency Graph — codesearch</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { display: flex; height: 100vh; overflow: hidden; background: #0d1117; color: #c9d1d9; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; font-size: 13px; }
  #sidebar { width: 240px; min-width: 180px; background: #161b22; border-right: 1px solid #30363d; display: flex; flex-direction: column; overflow: hidden; }
  #sidebar-inner { flex: 1; overflow-y: auto; padding: 14px; display: flex; flex-direction: column; gap: 14px; }
  h1 { font-size: 13px; font-weight: 600; color: #f0f6fc; }
  .section-label { font-size: 10px; font-weight: 600; color: #8b949e; text-transform: uppercase; letter-spacing: .6px; margin-bottom: 4px; }
  .repo-item { display: flex; align-items: center; gap: 8px; padding: 5px 8px; border-radius: 6px; cursor: pointer; transition: background .15s; }
  .repo-item:hover { background: #21262d; }
  .repo-item.active { background: #21262d; outline: 1px solid #30363d; }
  .repo-dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .repo-name { color: #c9d1d9; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; }
  input[type="text"] { width: 100%; padding: 6px 10px; border-radius: 6px; background: #0d1117; border: 1px solid #30363d; color: #c9d1d9; font-size: 12px; outline: none; }
  input[type="text"]:focus { border-color: #58a6ff; }
  .stat-row { display: flex; justify-content: space-between; font-size: 12px; }
  .stat-val { font-weight: 600; color: #58a6ff; }
  #hint { padding: 10px 14px; font-size: 10px; color: #6e7681; border-top: 1px solid #30363d; line-height: 1.6; }
  #graph-area { flex: 1; position: relative; overflow: hidden; }
  #sigma-container { width: 100%; height: 100%; }
  #tooltip { position: absolute; pointer-events: none; display: none; z-index: 100; background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 8px 12px; font-size: 11px; color: #c9d1d9; max-width: 300px; word-break: break-all; line-height: 1.5; box-shadow: 0 4px 12px rgba(0,0,0,0.4); }
  /* Detail panel */
  #detail-panel { position: absolute; right: 0; top: 0; bottom: 0; width: 300px; background: #161b22; border-left: 1px solid #30363d; display: none; flex-direction: column; z-index: 50; box-shadow: -4px 0 16px rgba(0,0,0,0.4); }
  #detail-header { display: flex; align-items: center; gap: 8px; padding: 12px 14px; border-bottom: 1px solid #30363d; min-height: 44px; }
  #detail-color { width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0; }
  #detail-title { font-size: 12px; font-weight: 600; color: #f0f6fc; flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  #detail-close { background: none; border: none; color: #8b949e; cursor: pointer; font-size: 20px; line-height: 1; padding: 0; flex-shrink: 0; }
  #detail-close:hover { color: #c9d1d9; }
  #detail-body { flex: 1; overflow-y: auto; padding: 12px 14px; display: flex; flex-direction: column; gap: 6px; }
  .d-meta { font-size: 11px; color: #8b949e; }
  .d-path { font-size: 10px; color: #6e7681; word-break: break-all; margin-bottom: 4px; }
  .d-sec { font-size: 10px; font-weight: 600; color: #8b949e; text-transform: uppercase; letter-spacing: .5px; margin-top: 8px; padding-bottom: 3px; border-bottom: 1px solid #21262d; }
  .d-repo-group { display: flex; flex-direction: column; gap: 1px; margin-top: 4px; }
  .d-repo-name { display: flex; align-items: center; gap: 6px; font-size: 12px; font-weight: 600; color: #c9d1d9; padding: 2px 0; }
  .d-dot { width: 7px; height: 7px; border-radius: 50%; display: inline-block; flex-shrink: 0; }
  .d-file { font-size: 11px; color: #8b949e; padding: 1px 0 1px 13px; display: flex; align-items: baseline; gap: 5px; }
  .d-file-repo { color: #6e7681; font-size: 10px; white-space: nowrap; }
  .d-more { font-size: 10px; color: #6e7681; padding-left: 13px; }
  .d-empty { font-size: 12px; color: #6e7681; font-style: italic; margin-top: 8px; }
</style>
</head>
<body>
<div id="sidebar">
  <div id="sidebar-inner">
    <h1>File Dependencies</h1>
    <div>
      <div class="section-label">Repositories</div>
      <div id="repo-list"></div>
    </div>
    <div>
      <div class="section-label">Search</div>
      <input type="text" id="search-input" placeholder="Filter by file path…">
    </div>
    <div>
      <div class="section-label">Stats</div>
      <div id="stats" style="display:flex;flex-direction:column;gap:3px;"></div>
    </div>
  </div>
  <div id="hint">Hover cluster → highlight connections<br>Click node → open detail panel<br>Scroll to zoom · drag to pan · click canvas to reset</div>
</div>
<div id="graph-area">
  <div id="sigma-container"></div>
  <div id="tooltip"></div>
  <div id="detail-panel">
    <div id="detail-header">
      <span id="detail-color"></span>
      <span id="detail-title"></span>
      <button id="detail-close">×</button>
    </div>
    <div id="detail-body"></div>
  </div>
</div>

<script src="https://cdn.jsdelivr.net/npm/graphology@0.25.4/dist/graphology.umd.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/sigma@2.4.0/build/sigma.min.js"></script>
<script>
// ── Utilities ──────────────────────────────────────────────────────────────────
function escHtml(s) { return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }
function hexRgb(h) { return [parseInt(h.slice(1,3),16),parseInt(h.slice(3,5),16),parseInt(h.slice(5,7),16)]; }
function dimColor(c) { if(!c)return '#161b22'; try{var r=hexRgb(c);return 'rgba('+r[0]+','+r[1]+','+r[2]+',0.12)';}catch(e){return '#161b22';} }
// shortName: show last 2 path components
function shortName(p) { var s=String(p).split('/'); return s.length>2?'\u2026/'+s.slice(-2).join('/'):p; }

// ── State ──────────────────────────────────────────────────────────────────────
var activeRepoId = null;

// ── Data ───────────────────────────────────────────────────────────────────────
const FILES = __FILES__;
const EDGES_DATA = __EDGES__;
const REPOS = __REPOS__;

const PALETTE = ['#388bfd','#3fb950','#d29922','#f78166','#a371f7','#ffa657','#79c0ff','#ff7b72','#56d364','#db6d28'];
const repoIds = Object.keys(REPOS);
const repoColor = {};
repoIds.forEach(function(id,i){ repoColor[id] = PALETTE[i % PALETTE.length]; });

// Build adjacency maps for the detail panel (once at startup)
var edgeInbound  = {};  // fileId → [{source, weight, kinds}]
var edgeOutbound = {};  // fileId → [{target, weight, kinds}]
EDGES_DATA.forEach(function(e) {
  if (!edgeOutbound[e.source]) edgeOutbound[e.source] = [];
  edgeOutbound[e.source].push(e);
  if (!edgeInbound[e.target]) edgeInbound[e.target] = [];
  edgeInbound[e.target].push(e);
});
var fileRepoMap = {};  // fileId → repoId
FILES.forEach(function(f){ fileRepoMap[f.id] = f.repoId; });

// ── Sidebar repo list ──────────────────────────────────────────────────────────
const repoList = document.getElementById('repo-list');
repoIds.forEach(function(id) {
  const repo = REPOS[id];
  const el = document.createElement('div');
  el.className = 'repo-item';
  el.dataset.repoId = id;
  el.innerHTML = '<div class="repo-dot" style="background:'+repoColor[id]+'"></div><span class="repo-name">'+escHtml(repo ? repo.name : id)+'</span>';
  el.addEventListener('click', function() { toggleRepo(id, el); });
  repoList.appendChild(el);
});

// ── Build graphology graph ─────────────────────────────────────────────────────
const graph = new graphology.Graph({ multi: false, type: 'directed' });

const REPO_RING_R = repoIds.length === 1 ? 0 : 650;
const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
const repoCenters = {};
repoIds.forEach(function(id, i) {
  var angle = (2 * Math.PI * i) / Math.max(repoIds.length, 1) - Math.PI / 2;
  repoCenters[id] = { x: REPO_RING_R * Math.cos(angle), y: REPO_RING_R * Math.sin(angle) };
});

var filesByRepo = {};
FILES.forEach(function(f) {
  if (!filesByRepo[f.repoId]) filesByRepo[f.repoId] = [];
  filesByRepo[f.repoId].push(f);
});

repoIds.forEach(function(id) {
  var c = repoCenters[id] || { x: 0, y: 0 };
  var n = (filesByRepo[id] || []).length;
  var repoName = REPOS[id] ? REPOS[id].name : id;
  graph.addNode('repo::' + id, {
    x: c.x, y: c.y,
    size: Math.max(20, Math.min(42, 10 + Math.sqrt(n) * 2.8)),
    color: repoColor[id] || '#58a6ff',
    label: repoName,
    nodeType: 'repo',
    repoId: id,
    origColor: repoColor[id] || '#58a6ff',
  });
});

FILES.forEach(function(f) {
  var center = repoCenters[f.repoId] || { x: 0, y: 0 };
  var repoFiles = filesByRepo[f.repoId] || [];
  var idx = repoFiles.indexOf(f);
  var n = repoFiles.length;
  var clusterR = n <= 1 ? 0 : Math.max(90, Math.sqrt(n) * 24);
  var r = n <= 1 ? 0 : clusterR * Math.sqrt((idx + 0.5) / n);
  var angle = idx * GOLDEN_ANGLE;
  graph.addNode(f.id, {
    x: center.x + r * Math.cos(angle),
    y: center.y + r * Math.sin(angle),
    size: 5,
    color: repoColor[f.repoId] || '#58a6ff',
    label: '',
    fullPath: f.id,
    shortLabel: f.label,
    nodeType: 'file',
    repoId: f.repoId,
    origColor: repoColor[f.repoId] || '#58a6ff',
  });
});

// Edges are hidden at rest (transparent); only revealed on hover
const EDGE_HIDDEN = '#0d1117';  // same as body background
const EDGE_DIM    = '#0d1117';

EDGES_DATA.forEach(function(e, i) {
  if (!graph.hasNode(e.source) || !graph.hasNode(e.target)) return;
  if (graph.hasEdge(e.source, e.target)) return;
  graph.addEdgeWithKey('e'+i, e.source, e.target, {
    size: 1,
    color: EDGE_HIDDEN,
    weight: e.weight,
    kinds: e.kinds,
  });
});

// ── Sigma ──────────────────────────────────────────────────────────────────────
const sigmaContainer = document.getElementById('sigma-container');
const renderer = new Sigma(graph, sigmaContainer, {
  renderEdgeLabels: false,
  defaultEdgeColor: EDGE_HIDDEN,
  defaultNodeColor: '#58a6ff',
  labelColor: { color: '#c9d1d9' },
  labelSize: 12,
  labelWeight: '600',
  labelRenderedSizeThreshold: 12,
  minCameraRatio: 0.04,
  maxCameraRatio: 14,
});

// ── Color helpers ──────────────────────────────────────────────────────────────
function restoreColors() {
  graph.forEachNode(function(n,a){ graph.setNodeAttribute(n,'color',a.origColor); });
  graph.forEachEdge(function(e){ graph.setEdgeAttribute(e,'color',EDGE_HIDDEN); });
}

function highlightNode(nodeKey) {
  var attrs = graph.getNodeAttributes(nodeKey);
  var hovRepoId = attrs.repoId;
  var lit = new Set();
  graph.forEachNode(function(n,a){ if (a.repoId === hovRepoId) lit.add(n); });
  graph.forEachNeighbor(nodeKey, function(n){ lit.add(n); });
  graph.forEachNode(function(n,a){
    graph.setNodeAttribute(n,'color', lit.has(n) ? a.origColor : dimColor(a.origColor));
  });
  var accent = repoColor[hovRepoId] || '#58a6ff';
  graph.forEachEdge(function(ek,a,src,tgt){
    var direct = (src === nodeKey || tgt === nodeKey);
    graph.setEdgeAttribute(ek,'color', direct ? accent : EDGE_HIDDEN);
    graph.setEdgeAttribute(ek,'size', direct ? 1 : 1);
  });
}

function highlightRepo(repoId) {
  var lit = new Set();
  graph.forEachNode(function(n,a){ if (a.repoId === repoId) lit.add(n); });
  graph.forEachNode(function(n,a){
    if (a.repoId === repoId) graph.forEachNeighbor(n, function(nb){ lit.add(nb); });
  });
  graph.forEachNode(function(n,a){
    graph.setNodeAttribute(n,'color', lit.has(n) ? a.origColor : dimColor(a.origColor));
  });
  var accent = repoColor[repoId] || '#58a6ff';
  graph.forEachEdge(function(ek,a,src,tgt){
    var sa = graph.getNodeAttributes(src), ta = graph.getNodeAttributes(tgt);
    var connected = (sa.repoId === repoId || ta.repoId === repoId);
    graph.setEdgeAttribute(ek,'color', connected ? accent : EDGE_HIDDEN);
  });
}

function isolateRepo(id) { highlightRepo(id); }

// ── Stats ──────────────────────────────────────────────────────────────────────
function updateStats() {
  document.getElementById('stats').innerHTML =
    '<div class="stat-row"><span>Files/dirs</span><span class="stat-val">'+FILES.length+'</span></div>'+
    '<div class="stat-row"><span>Connections</span><span class="stat-val">'+EDGES_DATA.length+'</span></div>'+
    '<div class="stat-row"><span>Repositories</span><span class="stat-val">'+repoIds.length+'</span></div>';
}
updateStats();

// ── Detail panel ───────────────────────────────────────────────────────────────
function openDetailPanel(nodeKey) {
  var attrs = graph.getNodeAttributes(nodeKey);
  var colorEl = document.getElementById('detail-color');
  var titleEl = document.getElementById('detail-title');
  var bodyEl  = document.getElementById('detail-body');
  var panel   = document.getElementById('detail-panel');
  var c = repoColor[attrs.repoId] || '#58a6ff';
  colorEl.style.background = c;

  if (attrs.nodeType === 'repo') {
    var rid = attrs.repoId;
    var files = filesByRepo[rid] || [];
    titleEl.textContent = attrs.label;
    var inboundByRepo = {}, outboundByRepo = {};
    files.forEach(function(f) {
      (edgeInbound[f.id] || []).forEach(function(e) {
        var sr = fileRepoMap[e.source];
        if (sr && sr !== rid) {
          if (!inboundByRepo[sr]) inboundByRepo[sr] = [];
          if (!inboundByRepo[sr].includes(e.source)) inboundByRepo[sr].push(e.source);
        }
      });
      (edgeOutbound[f.id] || []).forEach(function(e) {
        var tr = fileRepoMap[e.target];
        if (tr && tr !== rid) {
          if (!outboundByRepo[tr]) outboundByRepo[tr] = [];
          if (!outboundByRepo[tr].includes(e.target)) outboundByRepo[tr].push(e.target);
        }
      });
    });
    var html = '<div class="d-meta">'+files.length+' file'+(files.length!==1?'s':'')+'</div>';
    var inKeys = Object.keys(inboundByRepo);
    if (inKeys.length) {
      html += '<div class="d-sec">Used by</div>';
      inKeys.forEach(function(r) {
        var rn = REPOS[r] ? REPOS[r].name : r;
        var fl = inboundByRepo[r];
        html += '<div class="d-repo-group"><div class="d-repo-name"><span class="d-dot" style="background:'+repoColor[r]+'"></span>'+escHtml(rn)+'</div>';
        fl.slice(0,10).forEach(function(fid){ html += '<div class="d-file">'+escHtml(shortName(fid))+'</div>'; });
        if (fl.length>10) html += '<div class="d-more">+' +(fl.length-10)+' more</div>';
        html += '</div>';
      });
    }
    var outKeys = Object.keys(outboundByRepo);
    if (outKeys.length) {
      html += '<div class="d-sec">Uses</div>';
      outKeys.forEach(function(r) {
        var rn = REPOS[r] ? REPOS[r].name : r;
        var fl = outboundByRepo[r];
        html += '<div class="d-repo-group"><div class="d-repo-name"><span class="d-dot" style="background:'+repoColor[r]+'"></span>'+escHtml(rn)+'</div>';
        fl.slice(0,10).forEach(function(fid){ html += '<div class="d-file">'+escHtml(shortName(fid))+'</div>'; });
        if (fl.length>10) html += '<div class="d-more">+' +(fl.length-10)+' more</div>';
        html += '</div>';
      });
    }
    if (!inKeys.length && !outKeys.length) html += '<div class="d-empty">No cross-repo connections</div>';
    bodyEl.innerHTML = html;
  } else {
    // file node
    var fid = nodeKey;
    var rid = attrs.repoId;
    var rn = REPOS[rid] ? REPOS[rid].name : rid;
    titleEl.textContent = shortName(fid);
    var ins  = edgeInbound[fid]  || [];
    var outs = edgeOutbound[fid] || [];
    var html = '<div class="d-meta" style="color:'+c+'">'+escHtml(rn)+'</div>';
    html += '<div class="d-path">'+escHtml(fid)+'</div>';
    if (ins.length) {
      html += '<div class="d-sec">Referenced by ('+ins.length+')</div>';
      ins.slice(0,20).forEach(function(e) {
        var sr = fileRepoMap[e.source]; var srn = REPOS[sr] ? REPOS[sr].name : sr;
        html += '<div class="d-file"><span class="d-dot" style="background:'+(repoColor[sr]||'#58a6ff')+'"></span>'+
          escHtml(shortName(e.source))+'<span class="d-file-repo">'+escHtml(srn)+'</span></div>';
      });
      if (ins.length>20) html += '<div class="d-more">+'+(ins.length-20)+' more</div>';
    }
    if (outs.length) {
      html += '<div class="d-sec">References ('+outs.length+')</div>';
      outs.slice(0,20).forEach(function(e) {
        var tr = fileRepoMap[e.target]; var trn = REPOS[tr] ? REPOS[tr].name : tr;
        html += '<div class="d-file"><span class="d-dot" style="background:'+(repoColor[tr]||'#58a6ff')+'"></span>'+
          escHtml(shortName(e.target))+'<span class="d-file-repo">'+escHtml(trn)+'</span></div>';
      });
      if (outs.length>20) html += '<div class="d-more">+'+(outs.length-20)+' more</div>';
    }
    if (!ins.length && !outs.length) html += '<div class="d-empty">No connections</div>';
    bodyEl.innerHTML = html;
  }
  panel.style.display = 'flex';
}

function closeDetailPanel() {
  document.getElementById('detail-panel').style.display = 'none';
}

document.getElementById('detail-close').addEventListener('click', function() {
  closeDetailPanel();
  activeRepoId = null;
  document.querySelectorAll('.repo-item').forEach(function(r){r.classList.remove('active');});
  restoreColors();
});

// ── Tooltip ────────────────────────────────────────────────────────────────────
const tooltip = document.getElementById('tooltip');

renderer.on('enterNode', function(e) {
  var attrs = graph.getNodeAttributes(e.node);
  if (attrs.nodeType === 'repo') {
    var n = (filesByRepo[attrs.repoId] || []).length;
    tooltip.innerHTML = '<strong>'+escHtml(attrs.label)+'</strong><br>'+n+' file'+(n!==1?'s':'')+
      '<br><span style="color:#8b949e;font-size:10px">Click for details</span>';
    if (!activeRepoId) highlightRepo(attrs.repoId);
  } else {
    var rn = REPOS[attrs.repoId] ? REPOS[attrs.repoId].name : attrs.repoId;
    tooltip.innerHTML = '<strong>'+escHtml(attrs.fullPath || e.node)+'</strong>'+
      '<br><span style="color:'+repoColor[attrs.repoId]+'">'+escHtml(rn)+'</span>'+
      '<br><span style="color:#8b949e;font-size:10px">Click for details</span>';
    if (!activeRepoId) highlightNode(e.node);
  }
  tooltip.style.left = (e.event.x + 14) + 'px';
  tooltip.style.top  = (e.event.y + 14) + 'px';
  tooltip.style.display = 'block';
});

renderer.on('leaveNode', function() {
  tooltip.style.display = 'none';
  if (!activeRepoId) restoreColors();
});

// ── Click handlers ─────────────────────────────────────────────────────────────
renderer.on('clickNode', function(e) {
  var attrs = graph.getNodeAttributes(e.node);
  // Persist highlight and open detail panel
  activeRepoId = attrs.repoId;
  document.querySelectorAll('.repo-item').forEach(function(r){r.classList.remove('active');});
  var sidebarEl = document.querySelector('.repo-item[data-repo-id="'+attrs.repoId+'"]');
  if (sidebarEl) sidebarEl.classList.add('active');
  highlightRepo(attrs.repoId);
  openDetailPanel(e.node);
});

renderer.on('clickStage', function() {
  closeDetailPanel();
  activeRepoId = null;
  document.querySelectorAll('.repo-item').forEach(function(r){r.classList.remove('active');});
  restoreColors();
});

// ── Sidebar repo toggle ────────────────────────────────────────────────────────
function toggleRepo(id, el) {
  closeDetailPanel();
  if (activeRepoId === id) {
    activeRepoId = null;
    document.querySelectorAll('.repo-item').forEach(function(r){r.classList.remove('active');});
    restoreColors();
    return;
  }
  document.querySelectorAll('.repo-item').forEach(function(r){r.classList.remove('active');});
  el.classList.add('active');
  activeRepoId = id;
  isolateRepo(id);
}

// ── Search ─────────────────────────────────────────────────────────────────────
document.getElementById('search-input').addEventListener('input', function(e) {
  var q = e.target.value.trim().toLowerCase();
  closeDetailPanel();
  activeRepoId = null;
  document.querySelectorAll('.repo-item').forEach(function(r){r.classList.remove('active');});
  if (!q) { restoreColors(); return; }
  var matched = new Set();
  graph.forEachNode(function(n,a){ if((a.fullPath||a.label||'').toLowerCase().includes(q)) matched.add(n); });
  graph.forEachNode(function(n,a){
    graph.setNodeAttribute(n,'color', matched.has(n) ? a.origColor : dimColor(a.origColor));
  });
  graph.forEachEdge(function(ek,a,src,tgt){
    graph.setEdgeAttribute(ek,'color', matched.has(src)&&matched.has(tgt) ? '#58a6ff' : EDGE_HIDDEN);
  });
});
</script>
</body>
</html>
"#;
