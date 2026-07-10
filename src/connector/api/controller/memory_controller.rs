use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::application::{
    resource_slug, ChatClient, ImportOutcome, MEMORY_ROOT_URI, RESOURCES_ROOT_URI,
    SESSIONS_ROOT_URI,
};
use crate::cli::{LlmTarget, MemoryKindArg, OutputFormatTextJson};
use crate::connector::adapter::{
    discover_all_sessions, fetch_resource, load_transcript as load_discovered_transcript,
    parse_transcript_file, AnthropicClient, OpenAiChatClient,
};
use crate::domain::{MemoryItem, MemoryKind, MemoryNode, MemoryOperation};

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

    /// Build the chat client for the requested LLM provider.
    fn chat_client(llm: LlmTarget) -> Result<Arc<dyn ChatClient>> {
        Ok(match llm {
            LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
            LlmTarget::OpenAi => Arc::new(
                OpenAiChatClient::from_env()
                    .context("Failed to initialise OpenAI chat client for memory")?,
            ),
        })
    }

    pub async fn import(
        &self,
        path: Option<String>,
        llm: LlmTarget,
        force: bool,
    ) -> Result<String> {
        match path {
            Some(path) => {
                let transcript = parse_transcript_file(Path::new(&path))?;
                self.import_transcripts(vec![transcript], llm, force).await
            }
            None => self.import_via_picker(llm, force).await,
        }
    }

    /// Discover local sessions, show the interactive picker, and import the
    /// ones the user selects.
    async fn import_via_picker(&self, llm: LlmTarget, force: bool) -> Result<String> {
        let sessions = discover_all_sessions();
        if sessions.is_empty() {
            return Ok(
                "No sessions found from Claude Code, OpenCode, or Zed on this machine.\n\
                       Pass a transcript path to import one directly."
                    .to_string(),
            );
        }

        // The picker takes over the terminal; run it on a blocking thread so it
        // never shares the async runtime's reactor with the terminal loop. The
        // loader materializes a session's transcript on demand for the right
        // pane (lazy-loaded + cached inside the picker).
        let now = now_secs();
        let chosen = tokio::task::spawn_blocking(move || {
            let loader = |s: &crate::domain::DiscoveredSession| {
                load_discovered_transcript(s)
                    .map(|t| t.messages)
                    .map_err(|e| e.to_string())
            };
            crate::tui::import_picker::run(sessions, now, &loader)
        })
        .await
        .map_err(|e| anyhow::anyhow!("session picker task panicked: {e}"))??;

        if chosen.is_empty() {
            return Ok("Nothing selected — no sessions imported.".to_string());
        }

        // Materialize each chosen session's transcript, skipping any that fail
        // to load rather than aborting the whole batch.
        let mut transcripts = Vec::new();
        let mut load_errors = Vec::new();
        for session in &chosen {
            match load_discovered_transcript(session) {
                Ok(t) => transcripts.push(t),
                Err(e) => load_errors.push(format!(
                    "  ! {} ({}): {e}",
                    session.display_title(),
                    session.source
                )),
            }
        }

        let mut out = self.import_transcripts(transcripts, llm, force).await?;
        if !load_errors.is_empty() {
            out.push_str("\nSkipped (could not load):\n");
            out.push_str(&load_errors.join("\n"));
            out.push('\n');
        }
        Ok(out)
    }

    /// Run a batch of transcripts through the import pipeline, formatting a
    /// combined report.
    async fn import_transcripts(
        &self,
        transcripts: Vec<crate::domain::SessionTranscript>,
        llm: LlmTarget,
        force: bool,
    ) -> Result<String> {
        if transcripts.is_empty() {
            return Ok("Nothing to import.".to_string());
        }
        let chat_client = Self::chat_client(llm)?;
        let use_case = self.container.memory_import_use_case(chat_client)?;

        let multiple = transcripts.len() > 1;
        let mut output = String::new();
        for transcript in &transcripts {
            let outcome = use_case.execute(transcript, force).await?;
            output.push_str(&render_import_outcome(&outcome, multiple));
        }
        Ok(output)
    }

    pub async fn add_resource(
        &self,
        source: String,
        name: Option<String>,
        llm: LlmTarget,
    ) -> Result<String> {
        // Fetch first — a bad path/URL should fail before we spin up the LLM.
        let fetched = fetch_resource(&source)
            .await
            .with_context(|| format!("failed to fetch resource '{source}'"))?;

        // Name the node: an explicit --name wins, else derive from the title.
        let slug = resource_slug(name.as_deref().unwrap_or(&fetched.title));

        let chat_client = Self::chat_client(llm)?;
        let summary = self.container.memory_summary_use_case(chat_client)?;

        let node = summary
            .summarize_resource(&slug, &fetched.source, &fetched.text)
            .await?;
        // Keep the whole-memory rollup in sync (best-effort — the resource is
        // already stored, so a rollup hiccup must not fail the command).
        if let Err(e) = summary.regenerate_rollup().await {
            tracing::warn!("failed to regenerate memory rollup after `memory add`: {e}");
        }

        Ok(format!(
            "Added resource '{}' ({} chars) at {}\n\n{}",
            fetched.source,
            fetched.text.len(),
            node.uri(),
            node.abstract_()
        ))
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

        // A 'memory://' URI addresses a virtual-filesystem node (the memory
        // rollup, a stored session, …) rather than a flat item.
        if id.starts_with("memory://") {
            return match repo.find_node(&id).await? {
                Some(node) => Ok(render_node(&node)),
                None => Ok(format!("No memory node found at '{id}'.")),
            };
        }

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

    /// Browse the memory virtual filesystem. With no URI, show the top-level
    /// roots (rollup abstract + sessions/resources directories). With a
    /// directory URI, list its children and their one-line abstracts.
    pub async fn tree(&self, uri: Option<String>, format: OutputFormatTextJson) -> Result<String> {
        let repo = self.container.memory_repository()?;

        let (children, header) = match uri.as_deref() {
            // Root view: the rollup node plus each directory's children.
            None => {
                let mut nodes = Vec::new();
                if let Some(rollup) = repo.find_node(MEMORY_ROOT_URI).await? {
                    nodes.push(rollup);
                }
                nodes.extend(repo.list_child_nodes(SESSIONS_ROOT_URI).await?);
                nodes.extend(repo.list_child_nodes(RESOURCES_ROOT_URI).await?);
                (nodes, "Memory filesystem".to_string())
            }
            Some(dir) => (
                repo.list_child_nodes(dir).await?,
                format!("Children of {dir}"),
            ),
        };

        match format {
            OutputFormatTextJson::Json => Ok(serde_json::to_string_pretty(&children)?),
            OutputFormatTextJson::Text => {
                if children.is_empty() {
                    return Ok("Nothing here yet. Import a session with: \
                         codesearch memory import <transcript.jsonl>"
                        .to_string());
                }
                let mut output = format!("{header}:\n\n");
                for node in &children {
                    output.push_str(&format!("[{}] {}\n", node.kind(), node.uri()));
                    output.push_str(&format!(
                        "    {}\n\n",
                        preview(node.abstract_(), CONTENT_PREVIEW_CHARS)
                    ));
                }
                output.push_str("Drill in with: codesearch memory show <uri>\n");
                Ok(output)
            }
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

/// Current Unix time in seconds.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format one import outcome. When `multiple`, each session is prefixed so a
/// batch report stays readable.
fn render_import_outcome(outcome: &ImportOutcome, multiple: bool) -> String {
    match outcome {
        ImportOutcome::AlreadyImported { session } => {
            if multiple {
                format!("• {} — already imported (use --force)\n", session.id)
            } else {
                format!(
                    "Session '{}' was already imported ({} messages, {} items written). \
                     Use --force to re-import.\n",
                    session.id, session.message_count, session.items_written
                )
            }
        }
        ImportOutcome::Imported { session, report } => {
            let mut output = if multiple {
                format!(
                    "• {} ({} messages) — {} operation(s)\n",
                    session.id,
                    session.message_count,
                    report.applied.len()
                )
            } else {
                format!(
                    "Imported session '{}' ({} messages).\n",
                    session.id, session.message_count
                )
            };
            if report.applied.is_empty() && !multiple {
                output.push_str("No memories extracted — nothing durable in this session.\n");
            }
            let indent = if multiple { "    " } else { "  " };
            if !multiple || !report.applied.is_empty() {
                for op in &report.applied {
                    match op {
                        MemoryOperation::Upsert { kind, name, .. } => {
                            output.push_str(&format!("{indent}+ [{kind}] {name}\n"));
                        }
                        MemoryOperation::Delete { kind, name } => {
                            output.push_str(&format!("{indent}- [{kind}] {name}\n"));
                        }
                    }
                }
            }
            for (op, reason) in &report.skipped {
                let (kind, name) = match op {
                    MemoryOperation::Upsert { kind, name, .. }
                    | MemoryOperation::Delete { kind, name } => (kind, name),
                };
                output.push_str(&format!("{indent}~ [{kind}] {name} skipped: {reason}\n"));
            }
            output
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

/// Render a virtual-filesystem node with its L0 abstract, L1 overview, and L2
/// detail (present only for nodes that store content, e.g. session transcripts).
fn render_node(node: &MemoryNode) -> String {
    let mut out = format!("[{}] {}\n\n", node.kind(), node.uri());
    out.push_str(&format!("## Abstract (L0)\n{}\n\n", node.abstract_()));
    if !node.overview().trim().is_empty() {
        out.push_str(&format!("## Overview (L1)\n{}\n\n", node.overview()));
    }
    if !node.content().trim().is_empty() {
        out.push_str(&format!("## Detail (L2)\n{}\n", node.content()));
    }
    out
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
