use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::application::{ChatClient, ImportOutcome};
use crate::cli::{LlmTarget, MemoryKindArg, OutputFormatTextJson};
use crate::connector::adapter::{parse_transcript_file, AnthropicClient, OpenAiChatClient};
use crate::domain::{MemoryItem, MemoryKind, MemoryOperation};

use super::super::Container;

/// Characters of memory content shown per item in text output.
const CONTENT_PREVIEW_CHARS: usize = 160;

pub struct MemoryController<'a> {
    container: &'a Container,
}

impl<'a> MemoryController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn import(&self, path: String, llm: LlmTarget, force: bool) -> Result<String> {
        let chat_client: Arc<dyn ChatClient> = match llm {
            LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
            LlmTarget::OpenAi => Arc::new(
                OpenAiChatClient::from_env()
                    .context("Failed to initialise OpenAI chat client for memory import")?,
            ),
        };

        let transcript = parse_transcript_file(Path::new(&path))?;
        let use_case = self.container.memory_import_use_case(chat_client)?;
        let outcome = use_case.execute(&transcript, force).await?;

        match outcome {
            ImportOutcome::AlreadyImported { session } => Ok(format!(
                "Session '{}' was already imported ({} messages, {} items written). \
                 Use --force to re-import.",
                session.id, session.message_count, session.items_written
            )),
            ImportOutcome::Imported { session, report } => {
                let mut output = format!(
                    "Imported session '{}' ({} messages).\n",
                    session.id, session.message_count
                );
                if report.applied.is_empty() {
                    output.push_str("No memories extracted — nothing durable in this session.\n");
                } else {
                    output.push_str(&format!(
                        "{} memory operations applied:\n",
                        report.applied.len()
                    ));
                    for op in &report.applied {
                        match op {
                            MemoryOperation::Upsert { kind, name, .. } => {
                                output.push_str(&format!("  + [{kind}] {name}\n"));
                            }
                            MemoryOperation::Delete { kind, name } => {
                                output.push_str(&format!("  - [{kind}] {name}\n"));
                            }
                        }
                    }
                }
                for (op, reason) in &report.skipped {
                    let (kind, name) = match op {
                        MemoryOperation::Upsert { kind, name, .. }
                        | MemoryOperation::Delete { kind, name } => (kind, name),
                    };
                    output.push_str(&format!("  ~ [{kind}] {name} skipped: {reason}\n"));
                }
                Ok(output)
            }
        }
    }

    pub async fn search(
        &self,
        query: String,
        num: usize,
        kind: Option<MemoryKindArg>,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let use_case = self.container.memory_search_use_case()?;
        let kind = kind.map(MemoryKind::from);
        let results = use_case.execute(&query, kind, num).await?;

        match format {
            OutputFormatTextJson::Json => {
                let items: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(item, score)| {
                        let mut value = serde_json::to_value(item).unwrap_or_default();
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
                    return Ok("No memories found.".to_string());
                }
                let mut output = String::new();
                for (item, score) in &results {
                    output.push_str(&format!(
                        "[{:.3}] [{}] {} ({})\n",
                        score,
                        item.kind(),
                        item.name(),
                        item.id()
                    ));
                    output.push_str(&format!(
                        "    {}\n\n",
                        preview(item.content(), CONTENT_PREVIEW_CHARS)
                    ));
                }
                Ok(output)
            }
        }
    }

    pub async fn list(
        &self,
        kind: Option<MemoryKindArg>,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let repo = self.container.memory_repository()?;
        let items = repo.list_items(kind.map(MemoryKind::from)).await?;

        match format {
            OutputFormatTextJson::Json => Ok(serde_json::to_string_pretty(&items)?),
            OutputFormatTextJson::Text => {
                if items.is_empty() {
                    return Ok("No memories stored. Import a session with: \
                               codesearch memory import <transcript.jsonl>"
                        .to_string());
                }
                let mut output = format!("{} memories:\n\n", items.len());
                for item in &items {
                    output.push_str(&format!(
                        "[{}] {} ({})\n",
                        item.kind(),
                        item.name(),
                        item.id()
                    ));
                    output.push_str(&format!(
                        "    {}\n\n",
                        preview(item.content(), CONTENT_PREVIEW_CHARS)
                    ));
                }
                Ok(output)
            }
        }
    }

    pub async fn show(&self, id: String) -> Result<String> {
        let repo = self.container.memory_repository()?;

        // Accept '<kind>/<name>' as an alternative to the item ID.
        if let Some((kind_str, name)) = id.split_once('/') {
            if let Some(kind) = MemoryKind::parse(kind_str) {
                if let Some(item) = repo.find_item(kind, name).await? {
                    return Ok(render_item(&item));
                }
            }
        }

        let items = repo.list_items(None).await?;
        match items.iter().find(|i| i.id() == id) {
            Some(item) => Ok(render_item(item)),
            None => Ok(format!("No memory item found with ID '{id}'.")),
        }
    }

    pub async fn delete(&self, id: String) -> Result<String> {
        let repo = self.container.memory_repository()?;
        if repo.delete_item_by_id(&id).await? {
            Ok(format!("Deleted memory item '{id}'."))
        } else {
            Ok(format!("No memory item found with ID '{id}'."))
        }
    }

    pub async fn sessions(&self, format: OutputFormatTextJson) -> Result<String> {
        let repo = self.container.memory_repository()?;
        let sessions = repo.list_sessions().await?;

        match format {
            OutputFormatTextJson::Json => Ok(serde_json::to_string_pretty(&sessions)?),
            OutputFormatTextJson::Text => {
                if sessions.is_empty() {
                    return Ok("No sessions imported yet.".to_string());
                }
                let mut output = format!("{} imported sessions:\n\n", sessions.len());
                for session in &sessions {
                    output.push_str(&format!(
                        "{}\n    source: {}\n    messages: {}, items written: {}\n\n",
                        session.id, session.source, session.message_count, session.items_written
                    ));
                }
                Ok(output)
            }
        }
    }
}

fn render_item(item: &MemoryItem) -> String {
    format!(
        "[{}] {} ({})\nupdated {} time(s), source session: {}\n\n{}\n",
        item.kind(),
        item.name(),
        item.id(),
        item.update_count(),
        item.source_session_id().unwrap_or("(unknown)"),
        item.content()
    )
}

fn preview(content: &str, max_chars: usize) -> String {
    let single_line: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= max_chars {
        return single_line;
    }
    let truncated: String = single_line
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect();
    format!("{truncated}...")
}
