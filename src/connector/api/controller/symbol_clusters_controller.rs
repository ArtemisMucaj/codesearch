use anyhow::{Context, Result};

use crate::cli::{OutputFormat, OutputFormatTextJson};

use super::super::Container;

/// CLI controller for symbol-level communities (Leiden over the call graph).
pub struct SymbolClustersController<'a> {
    container: &'a Container,
}

impl<'a> SymbolClustersController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// List all symbol communities for the given repository.
    pub async fn list(&self, repository: String, format: OutputFormatTextJson) -> Result<String> {
        let use_case = self.container.symbol_cluster_detection_use_case();
        let graph = use_case
            .detect_communities(&repository)
            .await
            .context("detecting symbol communities for repository")?;

        let format: OutputFormat = format.into();
        Ok(match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&graph).context("serializing symbol communities")?
            }
            OutputFormat::Vimgrep => {
                anyhow::bail!("vimgrep output format is not supported for symbol-clusters list")
            }
            OutputFormat::Text => {
                if graph.communities.is_empty() {
                    return Ok(format!(
                        "No symbol communities detected for repository `{}` \
                         (the call graph may be empty — index with SCIP first).",
                        repository
                    ));
                }
                let mut out = format!(
                    "Symbol communities for `{}` — {} communities, {} symbols, {} edges\n\
                     ────────────────────────────────────────────────────────────\n",
                    repository,
                    graph.communities.len(),
                    graph.total_symbols,
                    graph.total_edges,
                );
                for (i, c) in graph.communities.iter().enumerate() {
                    out.push_str(&format!(
                        "{:>3}. {} ({} symbols, {}, cohesion {:.2})\n",
                        i + 1,
                        c.name,
                        c.size,
                        c.dominant_language,
                        c.cohesion,
                    ));
                    for m in c.members.iter().take(5) {
                        out.push_str(&format!("      {}\n", m));
                    }
                    if c.members.len() > 5 {
                        out.push_str(&format!("      … and {} more\n", c.members.len() - 5));
                    }
                }
                out
            }
        })
    }

    /// Show the community that a specific symbol belongs to.
    pub async fn get(
        &self,
        symbol: String,
        repository: String,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let use_case = self.container.symbol_cluster_detection_use_case();
        let result = use_case
            .community_for_symbol(&symbol, &repository)
            .await
            .context(format!(
                "finding community for symbol {} in repository {}",
                symbol, repository
            ))?;

        let format: OutputFormat = format.into();
        Ok(match (result, format) {
            (None, OutputFormat::Json) => serde_json::to_string_pretty(
                &serde_json::json!({ "symbol": symbol, "community": null, "repository": repository }),
            )
            .context("serializing not-found response")?,
            (None, OutputFormat::Vimgrep) => {
                anyhow::bail!("vimgrep output format is not supported for symbol-clusters get")
            }
            (None, OutputFormat::Text) => format!(
                "Symbol `{}` was not found in any community for repository `{}`.",
                symbol, repository
            ),
            (Some(c), OutputFormat::Json) => {
                serde_json::to_string_pretty(&c).context("serializing community")?
            }
            (Some(_), OutputFormat::Vimgrep) => {
                anyhow::bail!("vimgrep output format is not supported for symbol-clusters get")
            }
            (Some(c), OutputFormat::Text) => {
                let mut out = format!(
                    "Symbol `{}` belongs to community `{}` \
                     ({} symbols, {}, cohesion {:.2})\n",
                    symbol, c.name, c.size, c.dominant_language, c.cohesion
                );
                for m in c.members.iter().take(20) {
                    out.push_str(&format!("    {}\n", m));
                }
                if c.members.len() > 20 {
                    out.push_str(&format!("    … and {} more\n", c.members.len() - 20));
                }
                out
            }
        })
    }
}
