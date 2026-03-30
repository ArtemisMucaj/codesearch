use std::collections::HashMap;

use anyhow::Result;

use crate::cli::GraphFormat;
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
    ) -> Result<String> {
        let use_case = self.container.file_graph_use_case();
        let repo_ids = repository.as_deref();
        let graph = use_case
            .build_graph(repo_ids, min_weight, true)
            .await?;

        if graph.is_empty() {
            return Ok(
                "No file dependency edges found. Make sure at least one repository is indexed \
                 (codesearch index <path>) and has resolvable symbol references."
                    .to_string(),
            );
        }

        Ok(match format {
            GraphFormat::Dot => Self::format_dot(&graph),
            GraphFormat::Mermaid => Self::format_mermaid(&graph),
            GraphFormat::Json => serde_json::to_string_pretty(&graph)?,
        })
    }

    // ── DOT renderer ──────────────────────────────────────────────────────

    fn format_dot(graph: &FileGraph) -> String {
        let mut out = String::new();
        out.push_str(
            "digraph file_dependencies {\n\
             \trankdir=LR;\n\
             \tnode [shape=box fontname=\"Helvetica\" fontsize=10];\n\
             \tedge [fontsize=9];\n\n",
        );

        // Group files by repository so we can emit subgraph clusters.
        let mut repo_files: HashMap<&str, Vec<&str>> = HashMap::new();
        for file in &graph.files {
            // Determine which repo this file belongs to by searching edges.
            let repo_id = graph
                .edges
                .iter()
                .find_map(|e| {
                    if e.from_file == *file {
                        Some(e.from_repo_id.as_str())
                    } else if e.to_file == *file {
                        Some(e.to_repo_id.as_str())
                    } else {
                        None
                    }
                })
                .unwrap_or("");
            repo_files.entry(repo_id).or_default().push(file.as_str());
        }

        // Emit one subgraph cluster per repository.
        for (idx, (repo_id, files)) in repo_files.iter().enumerate() {
            let repo_name = graph
                .repositories
                .get(*repo_id)
                .map(|r| r.name.as_str())
                .unwrap_or(repo_id);

            out.push_str(&format!(
                "\tsubgraph cluster_{idx} {{\n\
                 \t\tlabel=\"{label}\";\n\
                 \t\tstyle=filled;\n\
                 \t\tcolor=lightblue;\n\
                 \t\tfontname=\"Helvetica Bold\";\n\
                 \t\tfontsize=12;\n\n",
                idx = idx,
                label = dot_escape(repo_name),
            ));

            let mut sorted_files = files.clone();
            sorted_files.sort();
            for file in sorted_files {
                out.push_str(&format!(
                    "\t\t{id} [label=\"{label}\"];\n",
                    id = dot_node_id(file),
                    label = dot_escape(short_path(file)),
                ));
            }
            out.push_str("\t}\n\n");
        }

        // Emit edges.
        for edge in &graph.edges {
            let label = if edge.weight > 1 {
                format!(" [label=\"{}\"]", edge.weight)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "\t{} -> {}{};\n",
                dot_node_id(&edge.from_file),
                dot_node_id(&edge.to_file),
                label,
            ));
        }

        out.push_str("}\n");
        out
    }

    // ── Mermaid renderer ──────────────────────────────────────────────────

    fn format_mermaid(graph: &FileGraph) -> String {
        let mut out = String::from("graph LR\n");

        // Group files by repository.
        let mut repo_files: HashMap<&str, Vec<&str>> = HashMap::new();
        for file in &graph.files {
            let repo_id = graph
                .edges
                .iter()
                .find_map(|e| {
                    if e.from_file == *file {
                        Some(e.from_repo_id.as_str())
                    } else if e.to_file == *file {
                        Some(e.to_repo_id.as_str())
                    } else {
                        None
                    }
                })
                .unwrap_or("");
            repo_files.entry(repo_id).or_default().push(file.as_str());
        }

        // Subgraphs (repository clusters).
        for (repo_id, files) in &repo_files {
            let repo_name = graph
                .repositories
                .get(*repo_id)
                .map(|r| r.name.as_str())
                .unwrap_or(repo_id);

            out.push_str(&format!("  subgraph {}\n", mermaid_escape(repo_name)));

            let mut sorted_files = files.clone();
            sorted_files.sort();
            for file in sorted_files {
                out.push_str(&format!(
                    "    {id}[\"{label}\"]\n",
                    id = mermaid_node_id(file),
                    label = mermaid_escape(short_path(file)),
                ));
            }
            out.push_str("  end\n");
        }

        out.push('\n');

        // Edges.
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

/// Shorten a file path to the last two components for display purposes.
fn short_path(path: &str) -> &str {
    // Find the second-to-last '/' and return everything after it, or the full
    // path if there are fewer than two components.
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
    let sanitised: String = path
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("n_{sanitised}")
}

/// Escape a string for use inside DOT double-quoted labels.
fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Convert a file path into a valid Mermaid node identifier (alphanumeric + _).
fn mermaid_node_id(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// Escape a string for use inside Mermaid double-quoted labels.
fn mermaid_escape(s: &str) -> String {
    s.replace('"', "'")
}

