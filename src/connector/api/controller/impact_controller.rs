use anyhow::Result;

use crate::cli::OutputFormat;
use crate::{CallGraphNode, ImpactAnalysis};

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
        repository: Option<String>,
        format: OutputFormat,
        is_regex: bool,
    ) -> Result<String> {
        let use_case = self.container.impact_use_case();
        let analysis = use_case
            .analyze(&symbol, repository.as_deref(), is_regex)
            .await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&analysis)?,
            OutputFormat::Vimgrep => Self::format_impact_vimgrep(&analysis),
            OutputFormat::Text => self.format_impact(&analysis),
        })
    }

    fn format_impact_vimgrep(analysis: &ImpactAnalysis) -> String {
        analysis
            .by_depth
            .iter()
            .flatten()
            .map(|node| {
                format!(
                    "{}:{}:1:{} [{}]",
                    node.file_path, node.line, node.symbol, node.reference_kind
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn format_impact(&self, analysis: &ImpactAnalysis) -> String {
        // "Not indexed" and "no callers" are opposite conclusions, and this is
        // what an agent reads before a refactor. Never print a zero-count blast
        // radius for a symbol that was never found.
        if !analysis.resolved {
            return format!(
                "Symbol '{}' was not found in the call graph — this is NOT a clean \
                 blast radius.\n\
                 Is this repository indexed with call-graph support? Try: codesearch index .",
                analysis.root_symbol
            );
        }

        if analysis.total_affected == 0 {
            return format!(
                "No callers found for '{}'. The symbol is indexed, so it is a root \
                 entry point — nothing calls it.",
                analysis.root_symbol
            );
        }

        let mut out = format!(
            "Impact analysis for '{}'\n\
             ─────────────────────────────────────────\n",
            analysis.root_symbol
        );

        // One chain per leaf: the leaf (nothing calls it) is the root of the
        // inverted tree, traced back down to the queried symbol.
        let chains = analysis.call_chains();

        for (idx, path) in chains.iter().enumerate() {
            Self::render_reversed_path(path, &analysis.root_symbol, &mut out);

            if idx < chains.len() - 1 {
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
    fn render_reversed_path(path: &[&CallGraphNode], root_symbol: &str, out: &mut String) {
        if path.is_empty() {
            return;
        }
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
