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

    // ── Interactive HTML renderer (Cytoscape.js) ──────────────────────────

    fn format_html(graph: &FileGraph, cluster: ClusterMode) -> String {
        let file_repo = Self::file_to_repo(graph);
        let consumer_map = Self::consumer_map(graph);
        let files_by_repo = Self::files_by_repo(graph);

        let mut nodes: Vec<serde_json::Value> = Vec::new();
        let mut edges_json: Vec<serde_json::Value> = Vec::new();

        // ── Repo compound nodes ──────────────────────────────────────────
        for (repo_id, repo) in &graph.repositories {
            nodes.push(serde_json::json!({
                "data": {
                    "id": format!("repo__{repo_id}"),
                    "label": repo.name,
                    "type": "repo",
                    "repoId": repo_id,
                }
            }));
        }

        // ── Optional intermediate sub-cluster nodes ──────────────────────
        match cluster {
            ClusterMode::Directory => {
                let mut seen: HashSet<String> = HashSet::new();
                for file in &graph.files {
                    let repo_id = file_repo.get(file.as_str()).copied().unwrap_or("");
                    let dir = file_dir(file);
                    if dir.is_empty() {
                        continue;
                    }
                    let id = format!("dir__{repo_id}__{dir}");
                    if seen.insert(id.clone()) {
                        nodes.push(serde_json::json!({
                            "data": {
                                "id": id,
                                "label": dir,
                                "type": "directory",
                                "parent": format!("repo__{repo_id}"),
                                "repoId": repo_id,
                            }
                        }));
                    }
                }
            }
            ClusterMode::Consumer => {
                let mut seen: HashSet<String> = HashSet::new();
                for (repo_id, files) in &files_by_repo {
                    for file in files {
                        let consumers: Vec<&str> = consumer_map
                            .get(file)
                            .map(|s| s.iter().copied().collect())
                            .unwrap_or_default();
                        let key = consumers.join(",");
                        let id = format!("cgrp__{repo_id}__{key}");
                        if seen.insert(id.clone()) {
                            let label = if consumers.is_empty() {
                                "internal".to_string()
                            } else {
                                let names: Vec<&str> = consumers
                                    .iter()
                                    .map(|cid| {
                                        graph
                                            .repositories
                                            .get(*cid)
                                            .map(|r| r.name.as_str())
                                            .unwrap_or(cid)
                                    })
                                    .collect();
                                format!("← {}", names.join(", "))
                            };
                            nodes.push(serde_json::json!({
                                "data": {
                                    "id": id,
                                    "label": label,
                                    "type": "consumer_group",
                                    "parent": format!("repo__{repo_id}"),
                                    "repoId": repo_id,
                                }
                            }));
                        }
                    }
                }
            }
            ClusterMode::None => {}
        }

        // ── File/directory leaf nodes ─────────────────────────────────────
        let mut sorted_files: Vec<&str> = graph.files.iter().map(String::as_str).collect();
        sorted_files.sort();

        for file in sorted_files {
            let repo_id = file_repo.get(file).copied().unwrap_or("");
            let parent = match cluster {
                ClusterMode::None => format!("repo__{repo_id}"),
                ClusterMode::Directory => {
                    let dir = file_dir(file);
                    if dir.is_empty() {
                        format!("repo__{repo_id}")
                    } else {
                        format!("dir__{repo_id}__{dir}")
                    }
                }
                ClusterMode::Consumer => {
                    let key = consumer_map
                        .get(file)
                        .map(|s| s.iter().copied().collect::<Vec<_>>().join(","))
                        .unwrap_or_default();
                    format!("cgrp__{repo_id}__{key}")
                }
            };
            nodes.push(serde_json::json!({
                "data": {
                    "id": format!("f__{file}"),
                    "label": short_path(file),
                    "fullPath": file,
                    "type": "file",
                    "parent": parent,
                    "repoId": repo_id,
                }
            }));
        }

        // ── Edges ─────────────────────────────────────────────────────────
        for (idx, edge) in graph.edges.iter().enumerate() {
            edges_json.push(serde_json::json!({
                "data": {
                    "id": format!("e{idx}"),
                    "source": format!("f__{}", edge.from_file),
                    "target": format!("f__{}", edge.to_file),
                    "weight": edge.weight,
                    "kinds": edge.reference_kinds.join(", "),
                }
            }));
        }

        let nodes_str = serde_json::to_string(&nodes).unwrap_or_default();
        let edges_str = serde_json::to_string(&edges_json).unwrap_or_default();
        let repos_str = serde_json::to_string(&graph.repositories).unwrap_or_default();
        let max_weight = graph.edges.iter().map(|e| e.weight).max().unwrap_or(1);

        HTML_TEMPLATE
            .replace("__NODES__", &nodes_str)
            .replace("__EDGES__", &edges_str)
            .replace("__REPOS__", &repos_str)
            .replace("__MAX_WEIGHT__", &max_weight.to_string())
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

// ── HTML template ─────────────────────────────────────────────────────────────

const HTML_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>File Dependency Graph — codesearch</title>
  <script src="https://cdn.jsdelivr.net/npm/cytoscape@3.29.2/dist/cytoscape.min.js"></script>
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      display: flex; height: 100vh; overflow: hidden;
      background: #0d1117; color: #c9d1d9;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      font-size: 13px;
    }
    #sidebar {
      width: 270px; min-width: 270px; padding: 16px 14px;
      background: #161b22; border-right: 1px solid #30363d;
      display: flex; flex-direction: column; gap: 14px; overflow-y: auto;
    }
    #sidebar h1 { font-size: 13px; font-weight: 600; color: #f0f6fc; letter-spacing: .3px; }
    .section { display: flex; flex-direction: column; gap: 6px; }
    .section-label { font-size: 10px; font-weight: 600; color: #8b949e; text-transform: uppercase; letter-spacing: .6px; }
    .repo-item {
      display: flex; align-items: center; gap: 8px;
      padding: 5px 8px; border-radius: 6px; cursor: pointer;
      transition: background .15s;
    }
    .repo-item:hover { background: #21262d; }
    .repo-item.active { background: #21262d; }
    .repo-dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
    .repo-name { color: #c9d1d9; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    input[type="text"] {
      width: 100%; padding: 6px 10px; border-radius: 6px;
      background: #0d1117; border: 1px solid #30363d; color: #c9d1d9;
      font-size: 12px; outline: none;
      transition: border-color .15s;
    }
    input[type="text"]:focus { border-color: #58a6ff; }
    .stat-row { display: flex; justify-content: space-between; }
    .stat-val { font-weight: 600; color: #58a6ff; }
    input[type="range"] { width: 100%; accent-color: #58a6ff; }
    .range-label { font-size: 11px; color: #8b949e; margin-top: 2px; }
    .hint { font-size: 10px; color: #484f58; line-height: 1.5; margin-top: auto; }
    #cy { flex: 1; }
    #tooltip {
      position: fixed; pointer-events: none; display: none; z-index: 999;
      background: #161b22; border: 1px solid #30363d; border-radius: 6px;
      padding: 6px 10px; font-size: 11px; color: #c9d1d9;
      max-width: 320px; word-break: break-all; line-height: 1.4;
    }
    #empty-msg {
      position: absolute; top: 50%; left: 50%; transform: translate(-50%, -50%);
      text-align: center; color: #484f58; pointer-events: none; display: none;
    }
  </style>
</head>
<body>
<div id="sidebar">
  <h1>📦 File Dependencies</h1>

  <div class="section">
    <div class="section-label">Repositories</div>
    <div id="repo-list"></div>
    <div style="font-size:11px;color:#484f58;padding-left:4px">Click to isolate</div>
  </div>

  <div class="section">
    <div class="section-label">Search</div>
    <input type="text" id="search" placeholder="Filter by file path…">
  </div>

  <div class="section">
    <div class="section-label">Stats</div>
    <div id="stats" style="display:flex;flex-direction:column;gap:3px;"></div>
  </div>

  <div class="section">
    <div class="section-label">Min edge weight</div>
    <input type="range" id="weight-slider" min="1" max="__MAX_WEIGHT__" value="1">
    <div class="range-label" id="weight-label">≥ 1 reference</div>
  </div>

  <div class="hint">
    Scroll to zoom · Drag to pan<br>
    Click a node to highlight neighbours<br>
    Click background to reset
  </div>
</div>

<div id="cy"></div>
<div id="tooltip"></div>

<script>
const NODES = __NODES__;
const EDGES = __EDGES__;
const REPOS = __REPOS__;
const MAX_WEIGHT = __MAX_WEIGHT__;

const COLOURS = [
  '#388bfd','#3fb950','#d29922','#f78166','#a371f7',
  '#39d353','#ffa657','#79c0ff','#ff7b72','#56d364'
];
const repoIds = Object.keys(REPOS);
const repoColour = {};
repoIds.forEach((id, i) => { repoColour[id] = COLOURS[i % COLOURS.length]; });

// ── Build Cytoscape stylesheet ────────────────────────────────────────────────
function makeStyle() {
  const base = [
    {
      selector: 'node[type="repo"]',
      style: {
        label: 'data(label)', 'text-valign': 'top', 'text-halign': 'center',
        'font-size': 13, 'font-weight': 'bold', 'color': '#f0f6fc',
        'background-opacity': .12, 'border-width': 1.5,
        padding: '18px', shape: 'round-rectangle',
      }
    },
    {
      selector: 'node[type="directory"], node[type="consumer_group"]',
      style: {
        label: 'data(label)', 'text-valign': 'top', 'text-halign': 'center',
        'font-size': 10, 'color': '#8b949e',
        'background-opacity': .06, 'border-width': 1, 'border-style': 'dashed',
        padding: '10px', shape: 'round-rectangle',
      }
    },
    {
      selector: 'node[type="file"]',
      style: {
        label: 'data(label)', shape: 'round-rectangle',
        width: 'label', height: 'label', padding: '7px',
        'font-size': 9, 'text-valign': 'center', 'text-halign': 'center',
        'color': '#0d1117', 'border-width': 0,
      }
    },
    {
      selector: 'edge',
      style: {
        'curve-style': 'bezier',
        'target-arrow-shape': 'triangle',
        width: 'mapData(weight, 1, ' + MAX_WEIGHT + ', 1, 5)',
        opacity: .6,
        'line-color': '#30363d', 'target-arrow-color': '#30363d',
      }
    },
    { selector: '.faded', style: { opacity: .05 } },
    { selector: '.highlighted', style: { opacity: 1 } },
    { selector: 'node:selected', style: { 'border-width': 2, 'border-color': '#f0f6fc' } },
  ];

  repoIds.forEach(id => {
    const c = repoColour[id];
    base.push({
      selector: `node[repoId="${id}"][type="repo"]`,
      style: { 'background-color': c, 'border-color': c, color: c }
    });
    base.push({
      selector: `node[repoId="${id}"][type="directory"]`,
      style: { 'border-color': c }
    });
    base.push({
      selector: `node[repoId="${id}"][type="file"]`,
      style: { 'background-color': c }
    });
  });

  return base;
}

// ── Init Cytoscape ────────────────────────────────────────────────────────────
let currentMinWeight = 1;

function visibleEdges() {
  return EDGES.filter(e => e.data.weight >= currentMinWeight);
}

const cy = cytoscape({
  container: document.getElementById('cy'),
  elements: [...NODES, ...visibleEdges()],
  style: makeStyle(),
  layout: {
    name: 'cose',
    animate: true,
    animationDuration: 600,
    nodeRepulsion: 12000,
    edgeElasticity: 100,
    idealEdgeLength: 80,
    nestingFactor: 1.2,
    gravity: 0.25,
    numIter: 1000,
    fit: true,
    padding: 40,
  },
  wheelSensitivity: 0.3,
});

// ── Sidebar: repository list ──────────────────────────────────────────────────
const repoList = document.getElementById('repo-list');
let activeRepo = null;

repoIds.forEach(id => {
  const repo = REPOS[id];
  const el = document.createElement('div');
  el.className = 'repo-item';
  el.dataset.repoId = id;
  el.innerHTML = `<div class="repo-dot" style="background:${repoColour[id]}"></div>
                  <span class="repo-name">${repo ? repo.name : id}</span>`;
  el.addEventListener('click', () => {
    if (activeRepo === id) {
      activeRepo = null;
      el.classList.remove('active');
      cy.elements().removeClass('faded highlighted');
      return;
    }
    document.querySelectorAll('.repo-item').forEach(r => r.classList.remove('active'));
    el.classList.add('active');
    activeRepo = id;
    const keep = cy.elements().filter(n => n.data('repoId') === id || n.data('source') && true);
    const repoNode = cy.$(`node[repoId="${id}"][type="repo"]`);
    const descendants = repoNode.descendants();
    const connected = descendants.connectedEdges();
    const neighbourhood = repoNode.union(descendants).union(connected).union(connected.connectedNodes());
    cy.elements().addClass('faded');
    neighbourhood.removeClass('faded').addClass('highlighted');
  });
  repoList.appendChild(el);
});

// ── Stats ─────────────────────────────────────────────────────────────────────
function updateStats() {
  const visNodes = cy.nodes('[type="file"]:visible').length;
  const visEdges = cy.edges(':visible').length;
  document.getElementById('stats').innerHTML =
    `<div class="stat-row"><span>Files/dirs</span><span class="stat-val">${visNodes}</span></div>
     <div class="stat-row"><span>Edges</span><span class="stat-val">${visEdges}</span></div>
     <div class="stat-row"><span>Repositories</span><span class="stat-val">${repoIds.length}</span></div>`;
}
updateStats();

// ── Search ────────────────────────────────────────────────────────────────────
document.getElementById('search').addEventListener('input', e => {
  const q = e.target.value.trim().toLowerCase();
  cy.elements().removeClass('faded highlighted');
  if (!q) return;
  const matched = cy.nodes('[type="file"]').filter(n =>
    (n.data('fullPath') || '').toLowerCase().includes(q)
  );
  cy.elements().addClass('faded');
  matched
    .union(matched.connectedEdges())
    .union(matched.connectedEdges().connectedNodes())
    .removeClass('faded')
    .addClass('highlighted');
});

// ── Weight slider ─────────────────────────────────────────────────────────────
document.getElementById('weight-slider').addEventListener('input', function() {
  currentMinWeight = parseInt(this.value);
  document.getElementById('weight-label').textContent =
    `\u2265 ${currentMinWeight} reference${currentMinWeight !== 1 ? 's' : ''}`;
  cy.remove('edge');
  cy.add(visibleEdges());
  updateStats();
});

// ── Click interactions ────────────────────────────────────────────────────────
cy.on('tap', 'node[type="file"]', e => {
  cy.elements().removeClass('highlighted faded');
  const n = e.target;
  const nb = n.union(n.openNeighborhood());
  cy.elements().not(nb).addClass('faded');
  nb.addClass('highlighted');
});
cy.on('tap', e => {
  if (e.target === cy) {
    cy.elements().removeClass('faded highlighted');
    document.querySelectorAll('.repo-item').forEach(r => r.classList.remove('active'));
    activeRepo = null;
  }
});

// ── Tooltip ───────────────────────────────────────────────────────────────────
const tooltip = document.getElementById('tooltip');
cy.on('mouseover', 'node[type="file"]', e => {
  tooltip.innerHTML = `<strong>${e.target.data('fullPath')}</strong>`;
  tooltip.style.display = 'block';
});
cy.on('mouseover', 'edge', e => {
  const d = e.target.data();
  tooltip.innerHTML = `<strong>${d.weight}</strong> reference${d.weight !== 1 ? 's' : ''}<br><span style="color:#8b949e">${d.kinds}</span>`;
  tooltip.style.display = 'block';
});
cy.on('mouseout', () => { tooltip.style.display = 'none'; });
cy.on('mousemove', e => {
  const oe = e.originalEvent;
  tooltip.style.left = (oe.clientX + 14) + 'px';
  tooltip.style.top  = (oe.clientY + 14) + 'px';
});
</script>
</body>
</html>
"#;
