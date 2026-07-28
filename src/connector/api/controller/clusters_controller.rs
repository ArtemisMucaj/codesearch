use anyhow::{Context, Result};

use crate::cli::{LlmTarget, OutputFormat, OutputFormatTextJson};
use crate::domain::community_label;

use super::super::Container;
use super::build_chat_client;

pub struct ClustersController<'a> {
    container: &'a Container,
}

impl<'a> ClustersController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// List all clusters for the given repository — or, with `global`, the
    /// namespace-wide clusters across every indexed repository.
    pub async fn list(
        &self,
        repository: Option<String>,
        global: bool,
        format: OutputFormatTextJson,
        llm: LlmTarget,
        no_llm: bool,
    ) -> Result<String> {
        let use_case = self.container.cluster_detection_use_case();
        let (scope, mut cg) = if global {
            let cg = use_case
                .create_namespace_clusters()
                .await
                .context("detecting namespace-wide clusters")?;
            ("namespace (all repositories)".to_string(), cg)
        } else {
            let repository_id = self
                .container
                .resolve_repository_id(repository.as_deref())
                .await;
            let cg = use_case
                .create_clusters(&repository_id)
                .await
                .context("detecting clusters for repository")?;
            (repository_id, cg)
        };

        // LLM naming runs by default (best-effort, cached by cluster id). It
        // probes the endpoint with one call and skips to id fallback if that
        // fails, so an unreachable endpoint costs one quick error, not a timeout
        // per cluster. `--no-llm` skips it outright. A chat-client build failure
        // (e.g. TLS init) is non-fatal here — degrade to ids rather than aborting
        // the listing.
        if !no_llm {
            match build_chat_client(llm, self.container.data_dir()) {
                Ok(chat) => {
                    self.container
                        .community_naming_use_case()
                        .name_clusters(&mut cg.clusters, chat.as_ref())
                        .await;
                }
                Err(e) => tracing::warn!("LLM naming disabled, showing ids: {e}"),
            }
        }

        let format: OutputFormat = format.into();
        Ok(match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&cg).context("serializing cluster graph")?
            }
            OutputFormat::Vimgrep => {
                anyhow::bail!("vimgrep output format is not supported for cluster list")
            }
            OutputFormat::Text => {
                if cg.clusters.is_empty() {
                    return Ok(format!(
                        "No clusters detected for `{}` \
                         (graph may be too small or have no edges).",
                        scope
                    ));
                }
                let mut out = format!(
                    "Clusters for `{}` — {} clusters, {} files, {} edges\n\
                     ────────────────────────────────────────────────────\n",
                    scope,
                    cg.clusters.len(),
                    cg.total_files,
                    cg.total_edges,
                );
                for (i, c) in cg.clusters.iter().enumerate() {
                    out.push_str(&format!(
                        "{:>3}. {} ({} files, {}, cohesion {:.2})\n",
                        i + 1,
                        community_label(&c.display_name, &c.id),
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

    /// Show which cluster a specific file belongs to.
    pub async fn get(
        &self,
        file_path: String,
        repository: Option<String>,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let repository_id = self
            .container
            .resolve_repository_id(repository.as_deref())
            .await;
        let use_case = self.container.cluster_detection_use_case();
        let result = use_case
            .cluster_for_file(&file_path, &repository_id)
            .await
            .context(format!(
                "finding cluster for file {} in repository {}",
                file_path, repository_id
            ))?;

        let format: OutputFormat = format.into();
        Ok(match (result, format) {
            (None, OutputFormat::Json) => serde_json::to_string_pretty(
                &serde_json::json!({ "file": file_path, "cluster": null, "repository": repository_id }),
            )
            .context("serializing not-found response")?,
            (None, OutputFormat::Vimgrep) => {
                anyhow::bail!("vimgrep output format is not supported for cluster get")
            }
            (None, OutputFormat::Text) => format!(
                "File `{}` was not found in any cluster for repository `{}`.",
                file_path, repository_id
            ),
            (Some(c), OutputFormat::Json) => {
                serde_json::to_string_pretty(&c).context("serializing cluster")?
            }
            (Some(_), OutputFormat::Vimgrep) => {
                anyhow::bail!("vimgrep output format is not supported for cluster get")
            }
            (Some(c), OutputFormat::Text) => format!(
                "File `{}` belongs to cluster `{}` \
                 ({} files, {}, cohesion {:.2})\n",
                file_path,
                community_label(&c.display_name, &c.id),
                c.size,
                c.dominant_language,
                c.cohesion
            ),
        })
    }
}
