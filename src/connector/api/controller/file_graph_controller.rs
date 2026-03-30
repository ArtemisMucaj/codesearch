use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;

use crate::cli::{ClusterMode, GraphFormat};
use crate::domain::FileGraph;

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
        min_weight: usize,
        cluster: ClusterMode,
    ) -> Result<String> {
        let use_case = self.container.file_graph_use_case();
        let repo_ids = repository.as_deref();
        let graph = use_case.build_graph(repo_ids, min_weight, true).await?;

        if graph.is_empty() {
            return Ok(
                "No file dependency edges found. Make sure at least one repository is indexed \
                 (codesearch index <path>) and has resolvable symbol references."
                    .to_string(),
            );
        }

        Ok(match format {
            GraphFormat::Dot => Self::format_dot(&graph, cluster),
            GraphFormat::Mermaid => Self::format_mermaid(&graph, cluster),
            GraphFormat::Json => serde_json::to_string_pretty(&graph)?,
        })
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    /// Map every file in the graph to the repo_id it belongs to.
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

    /// Group files by repo_id.
    fn files_by_repo<'g>(graph: &'g FileGraph) -> BTreeMap<&'g str, BTreeSet<&'g str>> {
        let file_repo = Self::file_to_repo(graph);
        let mut map: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (file, repo) in &file_repo {
            map.entry(repo).or_default().insert(file);
        }
        map
    }

    /// For each file, collect the set of repo_ids that have at least one
    /// edge pointing *to* that file (i.e. its consumers).
    fn consumer_map<'g>(graph: &'g FileGraph) -> HashMap<&'g str, BTreeSet<&'g str>> {
        let mut map: HashMap<&str, BTreeSet<&str>> = HashMap::new();
        for edge in &graph.edges {
            // Only cross-repo incoming edges are interesting for consumer grouping.
            if edge.from_repo_id != edge.to_repo_id {
                map.entry(edge.to_file.as_str())
                    .or_default()
                    .insert(edge.from_repo_id.as_str());
            }
        }
        map
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

        // Edges
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

    fn dot_emit_dir_subclusters(
        out: &mut String,
        repo_idx: usize,
        files: &BTreeSet<&str>,
    ) {
        // Group files by parent directory.
        let mut by_dir: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for file in files {
            let dir = file_dir(file);
            by_dir.entry(dir).or_default().push(file);
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
        repo_id: &str,
        files: &BTreeSet<&str>,
        consumers: &HashMap<&str, BTreeSet<&str>>,
        graph: &FileGraph,
    ) {
        // Group files by their consumer set.
        // Files with no external consumers go into an "internal / origin" group.
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
                // Files that are only sources (never a cross-repo target).
                format!("← (internal to {})", repo_id)
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
                color = if consumer_ids.is_empty() {
                    "white"
                } else {
                    "lightgreen"
                },
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
                id = mermaid_node_id(repo_id),
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
                        let dir_label = if dir.is_empty() { "/" } else { dir };
                        let dir_id =
                            format!("{}_{}", mermaid_node_id(repo_id), mermaid_node_id(dir_label));
                        out.push_str(&format!(
                            "    subgraph {id}[\"{label}\"]\n",
                            id = dir_id,
                            label = mermaid_escape(dir_label),
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
                    for (grp_idx, (consumer_ids, grp_files)) in
                        by_consumers.iter().enumerate()
                    {
                        let label = if consumer_ids.is_empty() {
                            format!("internal to {}", repo_name)
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
                        let grp_id =
                            format!("{}_c{}", mermaid_node_id(repo_id), grp_idx);
                        out.push_str(&format!(
                            "    subgraph {id}[\"{label}\"]\n",
                            id = grp_id,
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

        // Edges
        for edge in &graph.edges {
            let arrow = if edge.weight > 1 {
                format!(" -- {} --> ", edge.weight)
            } else {
                " --> ".to_string()
            };
            out.push_str(&format!(
                "  {}{}{}\n",
                mermaid_node_id(&edge.from_file),
                arrow,
                mermaid_node_id(&edge.to_file),
            ));
        }

        out
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the directory part of a path (everything before the last `/`).
fn file_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(pos) => &path[..pos],
        None => "",
    }
}

/// Emit a DOT node declaration with the given indentation.
fn dot_emit_node(out: &mut String, file: &str, indent: &str) {
    out.push_str(&format!(
        "{indent}{id} [label=\"{label}\"];\n",
        indent = indent,
        id = dot_node_id(file),
        label = dot_escape(short_path(file)),
    ));
}

/// Emit a Mermaid node declaration with the given indentation.
fn mermaid_emit_node(out: &mut String, file: &str, indent: &str) {
    out.push_str(&format!(
        "{indent}{id}[\"{label}\"]\n",
        indent = indent,
        id = mermaid_node_id(file),
        label = mermaid_escape(short_path(file)),
    ));
}

/// Shorten a file path to the last two components for display.
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

/// Convert a file path into a valid DOT node identifier.
fn dot_node_id(path: &str) -> String {
    let s: String = path
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("n_{s}")
}

/// Escape a string for DOT double-quoted labels.
fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Convert a string into a valid Mermaid identifier.
fn mermaid_node_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// Escape a string for Mermaid double-quoted labels.
fn mermaid_escape(s: &str) -> String {
    s.replace('"', "'")
}
