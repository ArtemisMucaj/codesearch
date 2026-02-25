use anyhow::Result;

use crate::cli::OutputFormat;
use crate::ImpactAnalysis;

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

        for (depth_idx, nodes) in analysis.by_depth.iter().enumerate() {
            if nodes.is_empty() {
                continue;
            }
            let depth = depth_idx + 1;
            let label = if depth == 1 {
                "direct callers".to_string()
            } else {
                format!("callers of depth-{} symbols", depth - 1)
            };
            out.push_str(&format!(
                "Depth {} — {} ({} symbol(s)):\n",
                depth,
                label,
                nodes.len()
            ));
            for node in nodes {
                let location = format!("{}:{}", node.file_path, node.line);
                let via = if depth > 1 {
                    node.via_symbol
                        .as_deref()
                        .map(|v| format!("  ← via {}", v))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "  • {} [{}]  {}{}\n",
                    node.symbol, node.reference_kind, location, via
                ));
            }
            out.push('\n');
        }

        out
    }
}
