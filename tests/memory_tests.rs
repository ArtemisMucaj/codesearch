//! Integration tests for the session-memory pipeline:
//! transcript parsing → LLM extraction (scripted) → DuckDB storage → search.
//!
//! Uses an in-memory memory database, mock embeddings, and a scripted chat
//! client, so no network or model download is required.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use codesearch::resource_slug;
use codesearch::{
    parse_transcript, ChatClient, DomainError, DuckdbMemoryRepository, EmbeddingService,
    ImportOutcome, ImportSessionUseCase, MemoryExtractionUseCase, MemoryKind, MemoryRepository,
    MemorySearchUseCase, MockEmbedding, NoEmbedding, NodeKind, SessionMessage, SessionTranscript,
    SummarizeMemoryUseCase, MEMORY_ROOT_URI, SESSIONS_ROOT_URI,
};

/// A canned `{abstract, overview}` reply for the summarization calls the
/// importer makes after extraction. Kept generic so summary calls never
/// consume the scripted *extraction* queue and never fail the import.
const SUMMARY_REPLY: &str = r#"{"abstract": "Test session summary.", "overview": "- did a thing"}"#;

/// Chat client that replays a fixed sequence of responses for *extraction*
/// calls, while answering *summarization* calls (session L0/L1 + rollup) with
/// a fixed valid reply. Routing is by system prompt so summary calls don't
/// drain the extraction script; only extraction calls are recorded.
struct ScriptedChatClient {
    responses: Mutex<Vec<String>>,
    calls: Mutex<Vec<(String, String)>>,
}

impl ScriptedChatClient {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Extraction calls recorded so far (summary calls are not recorded).
    async fn recorded_calls(&self) -> Vec<(String, String)> {
        self.calls.lock().await.clone()
    }
}

/// Whether a `complete` call is a summarization call rather than extraction.
/// The summarization system prompts describe summarizing a session / resource
/// / index.
fn is_summary_call(system: &str) -> bool {
    system.contains("summarize a finished coding-assistant session")
        || system.contains("summarize a document or web page")
        || system.contains("top-level index")
}

#[async_trait]
impl ChatClient for ScriptedChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        if is_summary_call(system) {
            return Ok(SUMMARY_REPLY.to_string());
        }
        self.calls
            .lock()
            .await
            .push((system.to_string(), user.to_string()));
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            return Err(DomainError::storage("no scripted response left"));
        }
        Ok(responses.remove(0))
    }
}

fn transcript(id: &str, messages: &[(&str, &str)]) -> SessionTranscript {
    SessionTranscript {
        id: id.to_string(),
        source: format!("{id}.jsonl"),
        messages: messages
            .iter()
            .map(|(role, content)| SessionMessage {
                role: role.to_string(),
                content: content.to_string(),
                timestamp: Some("2026-07-01T10:00:00Z".to_string()),
            })
            .collect(),
    }
}

fn extraction_json(preference: (&str, &str)) -> String {
    format!(
        r#"{{"preferences": [{{"name": "{}", "content": "{}"}}],
            "experiences": [], "skills": [], "facts": [], "delete": []}}"#,
        preference.0, preference.1
    )
}

struct Harness {
    memory_repo: Arc<dyn MemoryRepository>,
    embedding: Arc<dyn EmbeddingService>,
}

impl Harness {
    fn new() -> Self {
        Self {
            memory_repo: Arc::new(
                DuckdbMemoryRepository::in_memory(384, "mock-embedding").unwrap(),
            ),
            embedding: Arc::new(MockEmbedding::new()),
        }
    }

    fn import_use_case(&self, chat: Arc<ScriptedChatClient>) -> ImportSessionUseCase {
        let extraction = MemoryExtractionUseCase::new(
            Arc::clone(&chat) as Arc<dyn ChatClient>,
            Arc::clone(&self.memory_repo),
            Arc::clone(&self.embedding),
        );
        let summary = SummarizeMemoryUseCase::new(
            chat as Arc<dyn ChatClient>,
            Arc::clone(&self.memory_repo),
            Arc::clone(&self.embedding),
        );
        ImportSessionUseCase::new(Arc::clone(&self.memory_repo), extraction, summary)
    }
}

#[tokio::test]
async fn import_extracts_and_stores_memories() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r###"{"preferences": [{"name": "rust_error_handling", "content": "Prefers ? over unwrap in library code"}],
            "experiences": [{"name": "duckdb_lock_conflict_fix", "content": "## Situation\n- concurrent open\n## Approach\n- retry with backoff\n## Reflect\n- NEVER hold the write lock in read paths"}],
            "skills": [], "facts": [{"name": "project_uses_duckdb", "content": "The project stores all indexed data in DuckDB"}],
            "delete": []}"###,
    ]));
    let use_case = harness.import_use_case(Arc::clone(&chat));

    let transcript = transcript(
        "session-1",
        &[
            (
                "user",
                "Please never use unwrap in library code, use ? instead",
            ),
            ("assistant", "Understood, refactored to use ? everywhere."),
        ],
    );
    let outcome = use_case.execute(&transcript, false).await.unwrap();

    let ImportOutcome::Imported { session, report } = outcome else {
        panic!("expected Imported outcome");
    };
    assert_eq!(session.id, "session-1");
    assert_eq!(session.items_written, 3);
    assert_eq!(report.applied.len(), 3);

    // Items are stored and retrievable by kind.
    let prefs = harness
        .memory_repo
        .list_items(Some(MemoryKind::Preference))
        .await
        .unwrap();
    assert_eq!(prefs.len(), 1);
    assert_eq!(prefs[0].name(), "rust_error_handling");
    assert!(prefs[0].content().contains("?"));

    // The session marker is recorded.
    let session = harness
        .memory_repo
        .find_session("session-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.message_count, 2);

    // The prompt carried the conversation.
    let calls = chat.recorded_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].0.contains("memory extraction agent"));
    assert!(calls[0].1.contains("never use unwrap"));
}

#[tokio::test]
async fn import_is_idempotent_unless_forced() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        &extraction_json(("tabs_vs_spaces", "Prefers tabs")),
        &extraction_json(("tabs_vs_spaces", "Prefers tabs, strongly")),
    ]));
    let use_case = harness.import_use_case(chat);

    let transcript = transcript(
        "session-2",
        &[("user", "I prefer tabs"), ("assistant", "Noted.")],
    );

    let first = use_case.execute(&transcript, false).await.unwrap();
    assert!(matches!(first, ImportOutcome::Imported { .. }));

    // Second import without force is skipped (no LLM call consumed).
    let second = use_case.execute(&transcript, false).await.unwrap();
    assert!(matches!(second, ImportOutcome::AlreadyImported { .. }));

    // Forced re-import runs extraction again and rewrites the item.
    let third = use_case.execute(&transcript, true).await.unwrap();
    assert!(matches!(third, ImportOutcome::Imported { .. }));
    let item = harness
        .memory_repo
        .find_item(MemoryKind::Preference, "tabs_vs_spaces")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.content(), "Prefers tabs, strongly");
    assert_eq!(item.update_count(), 1);
}

#[tokio::test]
async fn extraction_recovers_from_malformed_output() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        "Sorry, here is some prose without JSON",
        &extraction_json(("commit_style", "Uses conventional commits")),
    ]));
    let use_case = harness.import_use_case(Arc::clone(&chat));

    let transcript = transcript(
        "session-3",
        &[("user", "use conventional commits"), ("assistant", "ok")],
    );
    let outcome = use_case.execute(&transcript, false).await.unwrap();
    let ImportOutcome::Imported { report, .. } = outcome else {
        panic!("expected Imported outcome");
    };
    assert_eq!(report.applied.len(), 1);
    // Two LLM calls: the failed one and the format-correction retry.
    assert_eq!(chat.recorded_calls().await.len(), 2);
}

#[tokio::test]
async fn delete_operation_removes_existing_item() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        &extraction_json(("old_fact", "The project uses SQLite")),
        r#"{"preferences": [], "experiences": [], "skills": [],
            "facts": [{"name": "storage_engine", "content": "The project migrated to DuckDB"}],
            "delete": [{"kind": "preference", "name": "old_fact"}]}"#,
    ]));
    let use_case = harness.import_use_case(chat);

    let first = transcript(
        "session-4a",
        &[("user", "we use sqlite"), ("assistant", "ok")],
    );
    use_case.execute(&first, false).await.unwrap();
    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Preference, "old_fact")
        .await
        .unwrap()
        .is_some());

    let second = transcript(
        "session-4b",
        &[("user", "we migrated to duckdb"), ("assistant", "ok")],
    );
    use_case.execute(&second, false).await.unwrap();
    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Preference, "old_fact")
        .await
        .unwrap()
        .is_none());
    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Fact, "storage_engine")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn hybrid_search_finds_stored_memories() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"preferences": [{"name": "python_typing", "content": "Dislikes type hints in Python, finds them redundant"}],
            "experiences": [], "skills": [],
            "facts": [{"name": "ci_provider", "content": "CI runs on GitHub Actions"}],
            "delete": []}"#,
    ]));
    let use_case = harness.import_use_case(chat);
    let transcript = transcript(
        "session-5",
        &[("user", "remove the type hints"), ("assistant", "done")],
    );
    use_case.execute(&transcript, false).await.unwrap();

    let search = MemorySearchUseCase::new(
        Arc::clone(&harness.memory_repo),
        Arc::clone(&harness.embedding),
    );
    let results = search.execute("type hints", None, 5).await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].0.name(), "python_typing");

    // Kind filter excludes non-matching kinds.
    let facts_only = search
        .execute("type hints", Some(MemoryKind::Fact), 5)
        .await
        .unwrap();
    assert!(facts_only.iter().all(|(i, _)| i.kind() == MemoryKind::Fact));
}

#[tokio::test]
async fn works_without_embeddings_via_keyword_search() {
    let memory_repo: Arc<dyn MemoryRepository> =
        Arc::new(DuckdbMemoryRepository::in_memory(384, "none").unwrap());
    let embedding: Arc<dyn EmbeddingService> = Arc::new(NoEmbedding::new(384));
    let chat = Arc::new(ScriptedChatClient::new(vec![&extraction_json((
        "editor_choice",
        "Uses Neovim with Telescope",
    ))]));
    let extraction = MemoryExtractionUseCase::new(
        Arc::clone(&chat) as Arc<dyn ChatClient>,
        Arc::clone(&memory_repo),
        Arc::clone(&embedding),
    );
    let summary = SummarizeMemoryUseCase::new(
        chat as Arc<dyn ChatClient>,
        Arc::clone(&memory_repo),
        Arc::clone(&embedding),
    );
    let use_case = ImportSessionUseCase::new(Arc::clone(&memory_repo), extraction, summary);

    let transcript = transcript(
        "session-6",
        &[("user", "I use neovim"), ("assistant", "noted")],
    );
    use_case.execute(&transcript, false).await.unwrap();

    let search = MemorySearchUseCase::new(memory_repo, embedding);
    let results = search.execute("neovim telescope", None, 5).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0.name(), "editor_choice");
}

#[tokio::test]
async fn transcript_parser_feeds_import_pipeline() {
    let content = r#"{"type":"user","sessionId":"cc-1","timestamp":"2026-07-01T09:00:00Z","message":{"role":"user","content":"Always run cargo fmt before committing"}}
{"type":"assistant","sessionId":"cc-1","timestamp":"2026-07-01T09:00:10Z","message":{"role":"assistant","content":[{"type":"text","text":"Will do."},{"type":"tool_use","name":"Bash","input":{"command":"cargo fmt"}}]}}"#;
    let transcript = parse_transcript(content, "fallback", "cc-1.jsonl").unwrap();
    assert_eq!(transcript.id, "cc-1");

    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![&extraction_json((
        "pre_commit_formatting",
        "Runs cargo fmt before every commit",
    ))]));
    let use_case = harness.import_use_case(Arc::clone(&chat));
    let outcome = use_case.execute(&transcript, false).await.unwrap();
    assert!(matches!(outcome, ImportOutcome::Imported { .. }));

    // Tool activity is visible to the extraction model as evidence.
    let calls = chat.recorded_calls().await;
    assert!(calls[0].1.contains("ToolCall: name=Bash"));
}

#[tokio::test]
async fn rejects_transcripts_with_too_few_messages() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![]));
    let use_case = harness.import_use_case(chat);
    let transcript = transcript("session-7", &[("user", "hi")]);
    let result = use_case.execute(&transcript, false).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn import_stores_session_node_with_full_transcript() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![&extraction_json((
        "editor",
        "Uses Neovim",
    ))]));
    let use_case = harness.import_use_case(chat);

    let transcript = transcript(
        "session-node-1",
        &[
            ("user", "I use neovim with telescope"),
            ("assistant", "Great choice."),
        ],
    );
    use_case.execute(&transcript, false).await.unwrap();

    // The session is stored as a node under memory://sessions with the full
    // transcript as its L2 detail and a generated L0 abstract.
    let uri = format!("{SESSIONS_ROOT_URI}/session-node-1");
    let node = harness
        .memory_repo
        .find_node(&uri)
        .await
        .unwrap()
        .expect("session node should exist");
    assert_eq!(node.kind(), NodeKind::Session);
    assert_eq!(node.parent_uri(), Some(SESSIONS_ROOT_URI));
    assert!(!node.abstract_().is_empty());
    // L2 preserves the actual conversation text.
    assert!(node.content().contains("neovim with telescope"));
    assert!(node.content().contains("Great choice."));

    // The session node is listed as a child of the sessions directory.
    let children = harness
        .memory_repo
        .list_child_nodes(SESSIONS_ROOT_URI)
        .await
        .unwrap();
    assert!(children.iter().any(|n| n.uri() == uri));
}

#[tokio::test]
async fn import_regenerates_whole_memory_rollup() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"preferences": [{"name": "a", "content": "one"}],
            "experiences": [], "skills": [],
            "facts": [{"name": "b", "content": "two"}], "delete": []}"#,
    ]));
    let use_case = harness.import_use_case(chat);
    let transcript = transcript(
        "session-rollup",
        &[("user", "remember these"), ("assistant", "ok")],
    );
    use_case.execute(&transcript, false).await.unwrap();

    // With ≥2 items the model-generated rollup is written at memory://memory.
    let rollup = harness
        .memory_repo
        .find_node(MEMORY_ROOT_URI)
        .await
        .unwrap()
        .expect("rollup node should exist");
    assert_eq!(rollup.kind(), NodeKind::Memory);
    assert_eq!(rollup.parent_uri(), None);
    assert!(!rollup.abstract_().is_empty());
}

#[tokio::test]
async fn add_resource_stores_node_with_full_text() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![]));
    let summary = SummarizeMemoryUseCase::new(
        chat as Arc<dyn ChatClient>,
        Arc::clone(&harness.memory_repo),
        Arc::clone(&harness.embedding),
    );

    let slug = resource_slug("Rust Error Handling Guide");
    let text = "# Error handling\n\nPrefer ? over unwrap in library code.";
    let node = summary
        .summarize_resource(&slug, "https://example.dev/guide", text)
        .await
        .unwrap();

    assert_eq!(node.kind(), NodeKind::Resource);
    assert_eq!(node.uri(), "memory://resources/rust_error_handling_guide");
    assert_eq!(node.parent_uri(), Some("memory://resources"));
    // Full text is preserved as L2.
    assert!(node.content().contains("Prefer ? over unwrap"));

    // The resource is listed under the resources directory.
    let children = harness
        .memory_repo
        .list_child_nodes("memory://resources")
        .await
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].uri(), node.uri());
}

#[tokio::test]
async fn summarize_without_embeddings_still_stores_nodes() {
    // No embeddings: nodes must still be written (keyword-searchable / browsable)
    // even though no vector is produced.
    let memory_repo: Arc<dyn MemoryRepository> =
        Arc::new(DuckdbMemoryRepository::in_memory(384, "none").unwrap());
    let embedding: Arc<dyn EmbeddingService> = Arc::new(NoEmbedding::new(384));
    let chat = Arc::new(ScriptedChatClient::new(vec![]));
    let summary = SummarizeMemoryUseCase::new(
        chat as Arc<dyn ChatClient>,
        Arc::clone(&memory_repo),
        Arc::clone(&embedding),
    );

    let transcript = transcript(
        "no-embed-session",
        &[("user", "hello there"), ("assistant", "hi")],
    );
    let node = summary.summarize_session(&transcript).await.unwrap();
    assert_eq!(node.kind(), NodeKind::Session);
    assert!(node.content().contains("hello there"));

    // Empty store → rollup falls back to a placeholder without an LLM call.
    let rollup = summary.regenerate_rollup().await.unwrap();
    assert_eq!(rollup.kind(), NodeKind::Memory);
    assert!(!rollup.abstract_().is_empty());
}
