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
    pub async fn list(
        &self,
        repository: Option<String>,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let repository_id = self
            .container
            .resolve_repository_id(repository.as_deref())
            .await;
        let use_case = self.container.symbol_cluster_detection_use_case();
        let graph = use_case
            .detect_communities(&repository_id)
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
                        repository_id
                    ));
                }
                let mut out = format!(
                    "Symbol communities for `{}` — {} communities, {} symbols, {} edges\n\
                     ────────────────────────────────────────────────────────────\n",
                    repository_id,
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
        repository: Option<String>,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let repository_id = self
            .container
            .resolve_repository_id(repository.as_deref())
            .await;
        let use_case = self.container.symbol_cluster_detection_use_case();
        let result = use_case
            .community_for_symbol(&symbol, &repository_id)
            .await
            .context(format!(
                "finding community for symbol {} in repository {}",
                symbol, repository_id
            ))?;

        let format: OutputFormat = format.into();
        Ok(match format {
            // Always serialize the `Option<SymbolCommunity>` (the community on a
            // hit, `null` on a miss) so `--format json` has one stable schema for
            // both outcomes, matching the `get_symbol_cluster` MCP tool.
            OutputFormat::Json => {
                serde_json::to_string_pretty(&result).context("serializing community")?
            }
            OutputFormat::Vimgrep => {
                anyhow::bail!("vimgrep output format is not supported for symbol-clusters get")
            }
            OutputFormat::Text => match result {
                None => format!(
                    "Symbol `{}` was not found in any community for repository `{}`.",
                    symbol, repository_id
                ),
                Some(c) => {
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
            },
        })
    }
}
