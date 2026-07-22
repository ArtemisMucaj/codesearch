//! CLI controller for the experimental claim graph: ingest transcripts into
//! the claim log and recall from the active-claim view.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::application::{ChatClient, IngestionOutcome};
use crate::cli::{LlmTarget, OutputFormatTextJson};
use crate::connector::adapter::parse_transcript_file;

use super::super::Container;

pub struct ClaimsController<'a> {
    container: &'a Container,
}

impl<'a> ClaimsController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// Memory project of the directory this command runs in (namespace, git
    /// remote, or `None`), resolved by the shared resolver.
    async fn current_dir_project(&self) -> Option<String> {
        let db_path = self.container.metadata_db_path();
        let cwd = std::env::current_dir().ok()?.to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || {
            crate::connector::api::repo_resolver::resolve_memory_project(Some(&db_path), &cwd)
        })
        .await
        .ok()
        .flatten()
    }

    fn chat_client(&self, llm: LlmTarget) -> Result<Arc<dyn ChatClient>> {
        super::build_chat_client(llm, self.container.data_dir())
    }

    /// `claims ingest <path>` — extract claims from a transcript into the graph.
    pub async fn ingest(&self, path: String, llm: LlmTarget, force: bool) -> Result<String> {
        let transcript =
            tokio::task::spawn_blocking(move || parse_transcript_file(Path::new(&path)))
                .await
                .map_err(|e| anyhow::anyhow!("transcript parse task panicked: {e}"))??;
        let chat = self.chat_client(llm)?;
        let use_case = self.container.claim_ingestion_use_case(chat)?;
        match use_case.execute(&transcript, force).await? {
            IngestionOutcome::AlreadyIngested => Ok(format!(
                "Session '{}' already has claims (use --force to re-ingest).",
                transcript.id
            )),
            IngestionOutcome::Ingested(report) => Ok(format!(
                "Ingested session '{}': {} claims, {} entities, {} edges.",
                transcript.id, report.claims_written, report.entities_created, report.edges_added
            )),
        }
    }

    /// `claims recall <query>` — graph-anchored hybrid recall over active claims.
    pub async fn recall(
        &self,
        query: String,
        num: usize,
        project: Option<String>,
        all_projects: bool,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let use_case = self.container.claim_recall_use_case()?;
        // Explicit --project wins; --all-projects disables filtering; otherwise
        // resolve from the current directory.
        let project = if all_projects {
            None
        } else if project.is_some() {
            project
        } else {
            self.current_dir_project().await
        };
        let results = use_case.execute(&query, project.as_deref(), num).await?;

        match format {
            OutputFormatTextJson::Json => {
                let items: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(claim, score)| {
                        let mut value = serde_json::to_value(claim).unwrap_or_default();
                        if let Some(obj) = value.as_object_mut() {
                            obj.insert("score".to_string(), serde_json::json!(score));
                        }
                        value
                    })
                    .collect();
                Ok(serde_json::to_string_pretty(&items)?)
            }
            OutputFormatTextJson::Text => {
                if results.is_empty() {
                    return Ok("No claims found.".to_string());
                }
                let mut output = String::new();
                for (claim, score) in &results {
                    let project_tag = claim
                        .project
                        .as_deref()
                        .map(|p| format!(" @{p}"))
                        .unwrap_or_default();
                    output.push_str(&format!(
                        "[{:.3}] {}{} ({})\n",
                        score, claim.statement, project_tag, claim.id
                    ));
                }
                Ok(output)
            }
        }
    }

    /// `claims stats` — counts of claims (by status), entities, and edges.
    pub async fn stats(&self, format: OutputFormatTextJson) -> Result<String> {
        let repo = self.container.claim_repository()?;
        let stats = repo.stats().await?;
        match format {
            OutputFormatTextJson::Json => {
                let by_status: Vec<serde_json::Value> = stats
                    .claims_by_status
                    .iter()
                    .map(|(status, count)| serde_json::json!({ "status": status, "count": count }))
                    .collect();
                Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "total_claims": stats.total_claims,
                    "claims_by_status": by_status,
                    "total_entities": stats.total_entities,
                    "total_edges": stats.total_edges,
                }))?)
            }
            OutputFormatTextJson::Text => {
                let mut out = format!("Claims: {}\n", stats.total_claims);
                for (status, count) in &stats.claims_by_status {
                    out.push_str(&format!("  {status}: {count}\n"));
                }
                out.push_str(&format!("Entities: {}\n", stats.total_entities));
                out.push_str(&format!("Edges: {}\n", stats.total_edges));
                Ok(out)
            }
        }
    }
}
