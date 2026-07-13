use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::application::{
    resource_slug, ChatClient, ImportOutcome, ImportSessionUseCase, MEMORY_ROOT_URI,
    RESOURCES_ROOT_URI, SESSIONS_ROOT_URI,
};
use crate::cli::{LlmTarget, MemoryKindArg, OutputFormatTextJson};
use crate::connector::adapter::{
    fetch_resource, load_transcript as load_discovered_transcript, parse_transcript_file,
};
use crate::domain::{DiscoveredSession, MemoryItem, MemoryKind, MemoryNode, MemoryOperation};
use crate::tui::import_picker::{ImportEvent, ImportRequest};

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

    /// Build the chat client for the requested LLM provider (shared dispatch).
    fn chat_client(&self, llm: LlmTarget) -> Result<Arc<dyn ChatClient>> {
        super::build_chat_client(llm, self.container.data_dir())
    }

    /// Import a single transcript file directly (the `memory import <path>`
    /// path). The no-path picker flow lives in [`run_import_picker_ui`] +
    /// [`Self::serve_import_requests`]: the picker opens *before* the container
    /// is built, and the worker imports selected sessions on demand while it
    /// stays open.
    pub async fn import(&self, path: String, llm: LlmTarget, force: bool) -> Result<String> {
        // Reading + parsing the transcript is blocking file I/O; keep it off the
        // async runtime thread.
        let transcript =
            tokio::task::spawn_blocking(move || parse_transcript_file(Path::new(&path)))
                .await
                .map_err(|e| anyhow::anyhow!("transcript parse task panicked: {e}"))??;
        self.import_transcripts(vec![transcript], llm, force).await
    }

    /// Service import requests from the interactive picker until the request
    /// channel closes (the user quit the picker).
    ///
    /// Runs as a background worker: it first reports the set of already-imported
    /// sessions (for the ✓ marks), then, for each request, materializes the
    /// transcript, runs extraction, and reports progress — all over `events`,
    /// so the picker stays open and live throughout.
    pub async fn serve_import_requests(
        &self,
        mut requests: tokio::sync::mpsc::UnboundedReceiver<ImportRequest>,
        events: std::sync::mpsc::Sender<ImportEvent>,
        llm: LlmTarget,
    ) -> Result<()> {
        // Announce readiness with the current imported-session set, so the
        // picker can mark them. A repo hiccup here just means no ✓ marks.
        let imported = self.imported_session_ids().await.unwrap_or_default();
        // If the picker already closed, there is nothing to serve.
        if events.send(ImportEvent::Ready { imported }).is_err() {
            return Ok(());
        }

        let chat_client = self.chat_client(llm)?;
        let use_case = self.container.memory_import_use_case(chat_client)?;

        // The picker sends requests one at a time (import-highlighted), so a
        // simple sequential loop keeps memory writes serialized and progress
        // easy to follow. `recv().await` yields the async worker while idle
        // instead of pinning a runtime thread.
        while let Some(ImportRequest { session }) = requests.recv().await {
            let id = (session.source.as_str().to_string(), session.id.clone());
            let _ = events.send(ImportEvent::Started { id: id.clone() });

            match self.import_one_discovered(&use_case, &session).await {
                Ok(summary) => {
                    let _ = events.send(ImportEvent::Done { id, summary });
                }
                Err(e) => {
                    let _ = events.send(ImportEvent::Failed {
                        id,
                        error: e.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Materialize one discovered session and run it through extraction,
    /// returning a one-line result summary. Re-imports are forced (the TUI
    /// re-runs extraction on demand).
    async fn import_one_discovered(
        &self,
        use_case: &ImportSessionUseCase,
        session: &DiscoveredSession,
    ) -> Result<String> {
        // Loading a transcript is blocking file/SQLite I/O; keep it off the
        // async worker thread.
        let title = session.display_title().to_string();
        let owned = session.clone();
        let transcript = tokio::task::spawn_blocking(move || load_discovered_transcript(&owned))
            .await
            .map_err(|e| anyhow::anyhow!("transcript load task panicked: {e}"))?
            .with_context(|| format!("could not load '{title}'"))?;
        let outcome = use_case.execute(&transcript, true).await?;
        Ok(import_outcome_summary(&outcome))
    }

    /// The identity set (`source`, `id`) of sessions already in the store, used
    /// to seed the picker's ✓ marks. The stored `source` is the transcript
    /// source string (`"opencode:<id>"`, a Claude file path, …); it is
    /// normalized to the bare source name (`"opencode"`, `"claude"`, `"zed"`)
    /// so the keys match the picker's `(source, id)` identity.
    async fn imported_session_ids(&self) -> Result<std::collections::HashSet<(String, String)>> {
        let repo = self.container.memory_repository()?;
        let sessions = repo.list_sessions().await?;
        Ok(sessions
            .into_iter()
            .map(|s| (normalize_source(&s.source), s.id))
            .collect())
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
        let chat_client = self.chat_client(llm)?;
        let use_case = self.container.memory_import_use_case(chat_client)?;

        let multiple = transcripts.len() > 1;
        let total = transcripts.len();
        let mut output = String::new();
        // Each session is one (slow) LLM extraction call. Emit per-session
        // progress via `tracing` (not raw stderr) so the CLI shows life without
        // this connector-layer code owning terminal output; the report itself is
        // returned to the router as the stdout value.
        for (idx, transcript) in transcripts.iter().enumerate() {
            tracing::info!(
                "extracting memories [{}/{}]: {}",
                idx + 1,
                total,
                transcript_label(transcript)
            );
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

        let chat_client = self.chat_client(llm)?;
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
                        "[{:.3}] [{}] {}{} ({})\n",
                        score,
                        item.kind(),
                        item.name(),
                        scope_tag(item),
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
                        "[{}] {}{} ({})\n",
                        item.kind(),
                        item.name(),
                        scope_tag(item),
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

        match repo.find_item_by_id(&id).await? {
            Some(item) => Ok(render_item(&item)),
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

        // Accept '<kind>/<name>' as an alternative to the item ID, matching
        // `show`, so `memory delete preference/tabs_vs_spaces` works.
        if let Some((kind_str, name)) = id.split_once('/') {
            if let Some(kind) = MemoryKind::parse(kind_str) {
                if repo.delete_item(kind, name).await? {
                    return Ok(format!("Deleted memory item '{id}'."));
                }
            }
        }

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

/// Run the interactive session-import picker to completion.
///
/// This is a free function — it needs **no** [`Container`] — so `main.rs` can
/// open the picker before building the container (and loading ONNX models). The
/// picker imports the highlighted session on demand by sending an
/// [`ImportRequest`] over `import_tx`; a background worker (see
/// [`MemoryController::serve_import_requests`]) processes it and reports back on
/// `events`. Discovery streams in on background threads, so the picker opens
/// immediately and fills in as each source (Claude / OpenCode / Zed) reports.
pub fn run_import_picker_ui(
    events: std::sync::mpsc::Receiver<ImportEvent>,
    import_tx: tokio::sync::mpsc::UnboundedSender<ImportRequest>,
) -> Result<()> {
    let now = now_secs();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<DiscoveredSession>>();

    // Kick off discovery; the picker drains `rx` as batches arrive. The thread
    // exits on its own when every source is done (or the receiver is dropped).
    std::thread::spawn(move || {
        crate::connector::adapter::discover_all_sessions_streaming(tx);
    });

    let loader = |s: &DiscoveredSession| {
        load_discovered_transcript(s)
            .map(|t| t.messages)
            .map_err(|e| e.to_string())
    };
    crate::tui::import_picker::run(rx, events, import_tx, now, &loader)
}

/// A one-line summary of an import outcome, for the picker footer.
fn import_outcome_summary(outcome: &ImportOutcome) -> String {
    match outcome {
        ImportOutcome::AlreadyImported { session } => {
            format!("{}: already imported", session.id)
        }
        ImportOutcome::Imported { session, report } => {
            let written = report.items_written();
            if written == 0 {
                format!("{}: nothing durable to remember", session.id)
            } else {
                format!(
                    "{}: {} memor{} written",
                    session.id,
                    written,
                    if written == 1 { "y" } else { "ies" }
                )
            }
        }
    }
}

/// Normalize a stored transcript `source` to the bare discovery source name so
/// it matches the picker's `(source, id)` key. Discovery sources prefix the
/// source (`"opencode:<id>"`, `"zed:<id>"`); Claude imports store a file path.
fn normalize_source(source: &str) -> String {
    if source.starts_with("opencode:") {
        "opencode".to_string()
    } else if source.starts_with("zed:") {
        "zed".to_string()
    } else {
        // Claude sessions store the transcript file path (or a raw id); the
        // picker keys Claude sessions as "claude".
        "claude".to_string()
    }
}

/// Short, human-readable label for a transcript, used in progress output.
/// Prefers the first non-empty user message; falls back to the session id.
fn transcript_label(transcript: &crate::domain::SessionTranscript) -> String {
    const LABEL_CHARS: usize = 60;
    let first_user = transcript
        .messages
        .iter()
        .find(|m| m.role == "user" && !m.content.trim().is_empty())
        .map(|m| m.content.trim());
    match first_user {
        Some(text) => preview(text, LABEL_CHARS),
        None => transcript.id.clone(),
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
    let scope = match item.scope() {
        Some(project) => format!(", scope: {project}"),
        None => ", scope: global".to_string(),
    };
    format!(
        "[{}] {} ({})\nupdated {} time(s), source session: {}{}\n\n{}\n",
        item.kind(),
        item.name(),
        item.id(),
        item.update_count(),
        item.source_session_id().unwrap_or("(unknown)"),
        scope,
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

/// A compact ` @project` suffix for a scoped memory, or empty for a global one.
fn scope_tag(item: &MemoryItem) -> String {
    match item.scope() {
        Some(project) => format!(" @{project}"),
        None => String::new(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_source_maps_to_bare_names() {
        // Matches the picker's `(source, id)` key derived from
        // SessionSource::as_str(): "claude" / "opencode" / "zed".
        assert_eq!(normalize_source("opencode:ses_abc123"), "opencode");
        assert_eq!(normalize_source("zed:thread-xyz"), "zed");
        // Claude imports store the transcript file path.
        assert_eq!(
            normalize_source("/Users/me/.claude/projects/-proj/uuid.jsonl"),
            "claude"
        );
        // A bare / unknown source defaults to claude.
        assert_eq!(normalize_source("uuid-only"), "claude");
    }
}
