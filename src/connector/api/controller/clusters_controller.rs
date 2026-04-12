use anyhow::{Context, Result};

use crate::cli::{OutputFormat, OutputFormatTextJson};

use super::super::Container;

pub struct ClustersController<'a> {
    container: &'a Container,
}

impl<'a> ClustersController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// List all clusters for the given repository.
    pub async fn list(
        &self,
        repository: String,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let use_case = self.container.cluster_detection_use_case();
        let cg = use_case
            .create_clusters(&repository)
            .await
            .context("detecting clusters for repository")?;

        let format: OutputFormat = format.into();
        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&cg)
                .context("serializing cluster graph")?,
            OutputFormat::Vimgrep => {
                anyhow::bail!("vimgrep output format is not supported for cluster list")
            }
            OutputFormat::Text => {
                if cg.clusters.is_empty() {
                    return Ok(format!(
                        "No clusters detected for repository `{}` \
                         (graph may be too small or have no edges).",
                        repository
                    ));
                }
                let mut out = format!(
                    "Clusters for `{}` — {} clusters, {} files, {} edges\n\
                     ────────────────────────────────────────────────────\n",
                    repository,
                    cg.clusters.len(),
                    cg.total_files,
                    cg.total_edges,
                );
                for (i, c) in cg.clusters.iter().enumerate() {
                    out.push_str(&format!(
                        "{:>3}. {} ({} files, {}, cohesion {:.2})\n",
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
                        out.push_str(&format!(
                            "      … and {} more\n",
                            c.members.len() - 5
                        ));
                    }
                }
                out
            }
        })
    }

    /// Show which cluster a specific file belongs to.
    pub async fn get(
        &self,
        file_path: String,
        repository: String,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let use_case = self.container.cluster_detection_use_case();
        let result = use_case
            .cluster_for_file(&file_path, &repository)
            .await
            .context(format!(
                "finding cluster for file {} in repository {}",
                file_path, repository
            ))?;

        let format: OutputFormat = format.into();
        Ok(match result {
            None => format!(
                "File `{}` was not found in any cluster for repository `{}`.",
                file_path, repository
            ),
            Some(c) => match format {
                OutputFormat::Json => serde_json::to_string_pretty(&c)
                    .context("serializing cluster")?,
                OutputFormat::Vimgrep => {
                    anyhow::bail!("vimgrep output format is not supported for cluster get")
                }
                OutputFormat::Text => format!(
                    "File `{}` belongs to cluster `{}` \
                     ({} files, {}, cohesion {:.2})\n",
                    file_path, c.name, c.size, c.dominant_language, c.cohesion
                ),
            },
        })
    }

    /// Print a Markdown architecture overview table.
    pub async fn overview(&self, repository: String) -> Result<String> {
        let use_case = self.container.cluster_detection_use_case();
        Ok(use_case
            .architecture_overview(&repository)
            .await
            .context("generating architecture overview")?)
    }
}