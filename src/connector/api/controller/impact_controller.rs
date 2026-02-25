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
             ─────────────────────────────────────────\n\
             Total affected symbols : {}\n\
             Max depth reached      : {}\n\n",
            analysis.root_symbol, analysis.total_affected, analysis.max_depth_reached
        );

        // Build a parent-symbol → children map using via_symbol.
        let all_nodes: Vec<&ImpactNode> = analysis.by_depth.iter().flatten().collect();
        let mut children_map: HashMap<&str, Vec<&ImpactNode>> = HashMap::new();
        for node in &all_nodes {
            if let Some(via) = node.via_symbol.as_deref() {
                children_map.entry(via).or_default().push(node);
            }
        }

        // Render root then recurse.
        out.push_str(&analysis.root_symbol);
        out.push('\n');
        let root_children = children_map
            .get(analysis.root_symbol.as_str())
            .cloned()
            .unwrap_or_default();
        Self::render_tree(&root_children, &children_map, "", &mut out);

        out
    }

    fn render_tree<'n>(
        nodes: &[&'n ImpactNode],
        children_map: &HashMap<&str, Vec<&'n ImpactNode>>,
        prefix: &str,
        out: &mut String,
    ) {
        for (i, node) in nodes.iter().enumerate() {
            let is_last = i == nodes.len() - 1;
            let connector = if is_last { "└── " } else { "├── " };
            let child_prefix = if is_last { "    " } else { "│   " };

            out.push_str(&format!(
                "{}{}{} [{}] {}:{}\n",
                prefix, connector, node.symbol, node.reference_kind, node.file_path, node.line,
            ));

            let children = children_map
                .get(node.symbol.as_str())
                .cloned()
                .unwrap_or_default();
            if !children.is_empty() {
                Self::render_tree(
                    &children,
                    children_map,
                    &format!("{}{}", prefix, child_prefix),
                    out,
                );
            }
        }
    }
}
