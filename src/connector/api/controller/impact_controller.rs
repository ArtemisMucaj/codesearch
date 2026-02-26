use std::collections::HashMap;

use anyhow::Result;

use crate::cli::OutputFormat;
use crate::{ImpactAnalysis, ImpactNode};

use super::super::Container;

pub struct ImpactController<'a> {
    container: &'a Container,
}

impl<'a> ImpactController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn impact(
        &self,
        symbol: String,
        depth: usize,
        repository: Option<String>,
        format: OutputFormat,
    ) -> Result<String> {
        let use_case = self.container.impact_use_case();
        let analysis = use_case
            .analyze(&symbol, depth, repository.as_deref())
            .await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&analysis)?,
            OutputFormat::Vimgrep => {
                anyhow::bail!("--format vimgrep is not supported for impact; use text or json")
            }
            OutputFormat::Text => self.format_impact(&analysis),
        })
    }

    fn format_impact(&self, analysis: &ImpactAnalysis) -> String {
        if analysis.total_affected == 0 {
            return format!(
                "No callers found for '{}'. Either the symbol is a root entry point or \
                 it hasn't been indexed yet.",
                analysis.root_symbol
            );
        }

        let mut out = format!(
            "Impact analysis for '{}'\n\
             ─────────────────────────────────────────\n",
            analysis.root_symbol
        );

        let all_nodes: Vec<&ImpactNode> = analysis.by_depth.iter().flatten().collect();

        // Build children_map: symbol → nodes that list it as via_symbol.
        let mut children_map: HashMap<&str, Vec<&ImpactNode>> = HashMap::new();
        for node in &all_nodes {
            if let Some(via) = node.via_symbol.as_deref() {
                children_map.entry(via).or_default().push(node);
            }
        }

        // Leaf nodes (no one calls them) become roots in the inverted tree.
        let leaf_nodes: Vec<&ImpactNode> = all_nodes
            .iter()
            .copied()
            .filter(|n| !children_map.contains_key(n.symbol.as_str()))
            .collect();

        // Lookup by (depth, symbol) for unambiguous path tracing.
        let mut node_by_depth_symbol: HashMap<(usize, &str), &ImpactNode> = HashMap::new();
        for node in &all_nodes {
            node_by_depth_symbol
                .entry((node.depth, node.symbol.as_str()))
                .or_insert(node);
        }

        for (idx, &leaf) in leaf_nodes.iter().enumerate() {
            // Trace from leaf back toward the root symbol.
            let mut path: Vec<&ImpactNode> = vec![leaf];
            let mut current = leaf;
            while let Some(via) = current.via_symbol.as_deref() {
                let parent_depth = current.depth.saturating_sub(1);
                if let Some(&parent) = node_by_depth_symbol.get(&(parent_depth, via)) {
                    path.push(parent);
                    current = parent;
                } else {
                    break;
                }
            }

            Self::render_reversed_path(&path, &analysis.root_symbol, &mut out);

            if idx < leaf_nodes.len() - 1 {
                out.push('\n');
            }
        }

        out
    }

    fn alias_suffix(alias: &Option<String>) -> String {
        alias
            .as_ref()
            .map(|a| format!(", as {}", a))
            .unwrap_or_default()
    }

    /// Render a single path (leaf → … → root) as an indented tree.
    /// `path[0]` is the most-upstream caller (tree root); the queried symbol
    /// is appended as the terminal leaf.
    fn render_reversed_path(path: &[&ImpactNode], root_symbol: &str, out: &mut String) {
        for (depth, node) in path.iter().enumerate() {
            let alias_suffix = Self::alias_suffix(&node.import_alias);
            if depth == 0 {
                out.push_str(&format!(
                    "{} [{}{}] {}:{}\n",
                    node.symbol, node.reference_kind, alias_suffix, node.file_path, node.line,
                ));
            } else {
                let indent = "    ".repeat(depth - 1);
                out.push_str(&format!(
                    "{}└── {} [{}{}] {}:{}\n",
                    indent,
                    node.symbol,
                    node.reference_kind,
                    alias_suffix,
                    node.file_path,
                    node.line,
                ));
            }
        }
        // Queried symbol is always the terminal leaf.
        let indent = "    ".repeat(path.len() - 1);
        out.push_str(&format!("{}└── {}\n", indent, root_symbol));
    }
}
