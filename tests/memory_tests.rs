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
    ImportOutcome, ImportSessionUseCase, MemoryBrowseUseCase, MemoryExtractionUseCase, MemoryKind,
    MemoryLevel, MemoryRepository, MemorySearchUseCase, MockEmbedding, NoEmbedding, NodeKind,
    RowTarget, SessionMessage, SessionTranscript, SummarizeMemoryUseCase, MEMORY_ROOT_URI,
    SESSIONS_ROOT_URI,
};

/// A canned `{abstract, overview}` reply for the summarization calls the
/// importer makes after extraction. Kept generic so summary calls never
/// consume the scripted *extraction* queue and never fail the import.
const SUMMARY_REPLY: &str = r#"{"abstract": "Test session summary.", "overview": "- did a thing"}"#;

/// Chat client that replays a fixed sequence of responses for *extraction*
/// calls, while answering *summarization* calls (session L0/L1 + digest) with
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
        || system.contains("about ONE project")
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
        project: None,
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
async fn find_item_by_id_round_trips() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![&extraction_json((
        "tabs_over_spaces",
        "Prefers tabs",
    ))]));
    harness
        .import_use_case(chat)
        .execute(
            &transcript(
                "session-id",
                &[("user", "I like tabs"), ("assistant", "ok")],
            ),
            false,
        )
        .await
        .unwrap();

    let stored = harness
        .memory_repo
        .find_item(MemoryKind::Preference, "tabs_over_spaces")
        .await
        .unwrap()
        .unwrap();

    // Look the same item up by its ID.
    let by_id = harness
        .memory_repo
        .find_item_by_id(stored.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_id.name(), "tabs_over_spaces");
    assert_eq!(by_id.id(), stored.id());

    // A missing ID yields None (not an error, not a scan).
    assert!(harness
        .memory_repo
        .find_item_by_id("no-such-id")
        .await
        .unwrap()
        .is_none());
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
    let results = search.execute("type hints", None, None, 5).await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].0.name(), "python_typing");

    // Kind filter restricts results to the requested kind. Query for the
    // fact's own content so the filter is exercised on a non-empty result set
    // (otherwise `all(...)` would pass vacuously on zero rows).
    let facts_only = search
        .execute("github actions ci", Some(MemoryKind::Fact), None, 5)
        .await
        .unwrap();
    assert!(
        facts_only.iter().any(|(i, _)| i.name() == "ci_provider"),
        "kind-filtered search should find the seeded fact"
    );
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
    let results = search
        .execute("neovim telescope", None, None, 5)
        .await
        .unwrap();
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
async fn import_regenerates_whole_memory_digest() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"preferences": [{"name": "a", "content": "one"}],
            "experiences": [], "skills": [],
            "facts": [{"name": "b", "content": "two"}], "delete": []}"#,
    ]));
    let use_case = harness.import_use_case(chat);
    let transcript = transcript(
        "session-digest",
        &[("user", "remember these"), ("assistant", "ok")],
    );
    use_case.execute(&transcript, false).await.unwrap();

    // With ≥2 items the model-generated digest is written at memory://memory.
    let digest = harness
        .memory_repo
        .find_node(MEMORY_ROOT_URI)
        .await
        .unwrap()
        .expect("digest node should exist");
    assert_eq!(digest.kind(), NodeKind::Memory);
    assert_eq!(digest.parent_uri(), None);
    assert!(!digest.abstract_().is_empty());
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
async fn browse_shows_filesystem_then_search_filters() {
    // Seed a store with an item (via import) and two nodes (a session from the
    // import + a resource) so the unified browse/search has both to work with.
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"preferences": [], "experiences": [], "skills": [],
            "facts": [{"name": "storage_engine", "content": "The project uses DuckDB for storage"}],
            "delete": []}"#,
    ]));
    harness
        .import_use_case(chat)
        .execute(
            &transcript(
                "browse-session",
                &[("user", "we use duckdb"), ("assistant", "noted")],
            ),
            false,
        )
        .await
        .unwrap();

    let summary = SummarizeMemoryUseCase::new(
        Arc::new(ScriptedChatClient::new(vec![])) as Arc<dyn ChatClient>,
        Arc::clone(&harness.memory_repo),
        Arc::clone(&harness.embedding),
    );
    summary
        .summarize_resource(
            "duckdb_guide",
            "https://x.dev/duckdb",
            "DuckDB locking uses a fixed read-only snapshot.",
        )
        .await
        .unwrap();

    let browse = MemoryBrowseUseCase::new(
        Arc::clone(&harness.memory_repo),
        Arc::clone(&harness.embedding),
    );

    // Empty query = browse: the whole virtual filesystem as a tree. The digest
    // leads at depth 0, with its L0/L1 as nested child rows; sessions and
    // resources each get a directory header with node rows + level children.
    let all = browse.execute("", 50).await.unwrap();
    assert!(
        matches!(&all[0].target, RowTarget::Node(node) if node.uri() == "memory://memory"),
        "digest node should be the first browse row"
    );
    // The digest's level rows are nested directly beneath it.
    assert!(
        matches!(
            &all[1].target,
            RowTarget::NodeLevel {
                level: MemoryLevel::Abstract,
                ..
            }
        ) && all[1].depth == all[0].depth + 1,
        "the row after the digest is its nested L0 level"
    );

    let has_dir = |name: &str| {
        all.iter()
            .any(|r| matches!(&r.target, RowTarget::Directory) && r.label == name)
    };
    assert!(has_dir("sessions/"), "sessions directory header present");
    assert!(has_dir("resources/"), "resources directory header present");
    // Items are grouped by category; the seeded fact lands in a `facts/`
    // sub-directory nested under the `memory://memory` digest (depth 1),
    // alongside the digest's L0/L1 levels — not in a separate top-level dir.
    assert!(has_dir("facts/"), "facts category sub-directory present");

    let digest_at = all
        .iter()
        .position(|r| matches!(&r.target, RowTarget::Node(n) if n.uri() == "memory://memory"))
        .unwrap();
    let facts_at = all
        .iter()
        .position(|r| matches!(&r.target, RowTarget::Directory) && r.label == "facts/")
        .unwrap();
    let item_at = all
        .iter()
        .position(|r| matches!(&r.target, RowTarget::Item(_)))
        .unwrap();
    // Order: digest → its category dir → the item, all before sessions/.
    assert!(digest_at < facts_at && facts_at < item_at, "nesting order");
    assert_eq!(all[digest_at].depth, 0, "digest at root");
    assert_eq!(all[facts_at].depth, 1, "category nested under the digest");
    assert_eq!(all[item_at].depth, 2, "item under its category");

    let has_session_node = all
        .iter()
        .any(|r| matches!(&r.target, RowTarget::Node(n) if n.kind() == NodeKind::Session));
    let has_resource_node = all
        .iter()
        .any(|r| matches!(&r.target, RowTarget::Node(n) if n.kind() == NodeKind::Resource));
    let has_l2 = all.iter().any(|r| {
        matches!(
            &r.target,
            RowTarget::NodeLevel {
                level: MemoryLevel::Detail,
                ..
            }
        )
    });
    let has_item = all.iter().any(|r| matches!(&r.target, RowTarget::Item(_)));
    assert!(
        has_session_node && has_resource_node && has_l2 && has_item,
        "browse tree includes session/resource nodes, an L2 level, and items"
    );

    // Non-empty query = search: a flat ranked list (no directory rows, no tree
    // depth), scored, and not led by the digest like browse is.
    let hits = browse.execute("duckdb storage engine", 50).await.unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits.iter().all(|r| r.depth == 0),
        "search rows are flat (depth 0)"
    );
    assert!(
        hits.iter().all(|r| !matches!(
            &r.target,
            RowTarget::Directory | RowTarget::NodeLevel { .. }
        )),
        "search rows are nodes/items, not directories or level rows"
    );
    assert!(
        hits.iter().all(|r| r.score.is_some_and(|s| s > 0.0)),
        "search rows are scored"
    );
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

    // Empty store → digest falls back to a placeholder without an LLM call.
    let digest = summary.regenerate_digest().await.unwrap();
    assert_eq!(digest.kind(), NodeKind::Memory);
    assert!(!digest.abstract_().is_empty());
}

// ─── Dream (offline consolidation) ──────────────────────────────────────────

use codesearch::{
    DiscoveredSession, MemoryDreamUseCase, MemoryItem, MemoryOperation, SessionDiscovery,
    SessionLocator, SessionSource,
};

/// Scripted [`SessionDiscovery`] source for harvest tests.
struct StubDiscovery {
    sessions: Vec<DiscoveredSession>,
    transcripts: std::collections::HashMap<String, SessionTranscript>,
}

impl StubDiscovery {
    fn empty() -> Self {
        Self {
            sessions: Vec::new(),
            transcripts: std::collections::HashMap::new(),
        }
    }
}

#[async_trait]
impl SessionDiscovery for StubDiscovery {
    async fn discover(&self) -> Result<Vec<DiscoveredSession>, DomainError> {
        Ok(self.sessions.clone())
    }

    async fn load_transcript(
        &self,
        session: &DiscoveredSession,
    ) -> Result<SessionTranscript, DomainError> {
        self.transcripts
            .get(&session.id)
            .cloned()
            .ok_or_else(|| DomainError::invalid_input("no stubbed transcript"))
    }
}

impl Harness {
    fn dream_use_case(
        &self,
        chat: Arc<ScriptedChatClient>,
        discovery: StubDiscovery,
    ) -> MemoryDreamUseCase {
        let import = self.import_use_case(Arc::clone(&chat));
        let summary = SummarizeMemoryUseCase::new(
            Arc::clone(&chat) as Arc<dyn ChatClient>,
            Arc::clone(&self.memory_repo),
            Arc::clone(&self.embedding),
        );
        MemoryDreamUseCase::new(
            Arc::clone(&self.memory_repo),
            chat as Arc<dyn ChatClient>,
            Arc::clone(&self.embedding),
            Arc::new(discovery),
            import,
            summary,
        )
    }

    /// Seed one item with a handcrafted embedding so clustering is controllable.
    async fn seed_item(&self, kind: MemoryKind, name: &str, content: &str, vector: &[f32]) {
        let item = MemoryItem::new(
            format!("id-{name}"),
            kind,
            name.to_string(),
            content.to_string(),
            None,
            None,
            100,
            100,
            0,
        );
        self.memory_repo
            .upsert_item(&item, Some(vector))
            .await
            .unwrap();
    }
}

/// A 384-dim unit vector along axis `axis`, tilted by `tilt` toward the next
/// axis. `tilt = 0.0` gives orthogonal vectors (never clustered); a small tilt
/// keeps two vectors on the same axis highly similar (always clustered).
fn test_vector(axis: usize, tilt: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; 384];
    v[axis] = 1.0;
    v[(axis + 1) % 384] = tilt;
    v
}

fn discovered(id: &str, updated_at: i64) -> DiscoveredSession {
    DiscoveredSession {
        source: SessionSource::Claude,
        id: id.to_string(),
        title: id.to_string(),
        cwd: None,
        updated_at,
        message_count: 2,
        tail_preview: String::new(),
        approx_tokens: 10,
        locator: SessionLocator::File(format!("{id}.jsonl")),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[tokio::test]
async fn dream_consolidates_duplicate_cluster() {
    let harness = Harness::new();
    // Two takes on the same topic, embedded close together → one cluster.
    harness
        .seed_item(
            MemoryKind::Experience,
            "db_lock_fix",
            "Retry with backoff fixes write-lock conflicts",
            &test_vector(0, 0.05),
        )
        .await;
    harness
        .seed_item(
            MemoryKind::Experience,
            "db_lock_retry",
            "Lock conflicts vanish when writers retry",
            &test_vector(0, 0.10),
        )
        .await;

    // The consolidation model merges both into one canonical item and deletes
    // the absorbed one.
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"items": [{"kind": "experience", "name": "db_lock_fix",
            "content": "Write-lock conflicts: writers must retry with backoff.", "project": null}],
            "delete": [{"kind": "experience", "name": "db_lock_retry"}]}"#,
    ]));
    let dream = harness.dream_use_case(Arc::clone(&chat), StubDiscovery::empty());

    let report = dream.execute(3_600, true).await.unwrap();

    assert_eq!(report.clusters_found, 1);
    assert_eq!(report.applied.len(), 2, "one merge upsert + one delete");
    let merged = harness
        .memory_repo
        .find_item(MemoryKind::Experience, "db_lock_fix")
        .await
        .unwrap()
        .expect("canonical item kept");
    assert!(merged.content().contains("retry with backoff"));
    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Experience, "db_lock_retry")
        .await
        .unwrap()
        .is_none());
    // The run is recorded for scheduling and status.
    let run = harness
        .memory_repo
        .last_dream_run()
        .await
        .unwrap()
        .expect("dream run recorded");
    assert_eq!(run.operations_applied, 2);
}

#[tokio::test]
async fn dream_always_runs_a_full_cycle() {
    let harness = Harness::new();
    // Two takes on the same topic → one cluster, examined on every cycle.
    harness
        .seed_item(
            MemoryKind::Experience,
            "dup_a",
            "first take",
            &test_vector(0, 0.05),
        )
        .await;
    harness
        .seed_item(
            MemoryKind::Experience,
            "dup_b",
            "second take",
            &test_vector(0, 0.10),
        )
        .await;

    // The model finds nothing to change on either cycle.
    let no_op = r#"{"items": [], "delete": []}"#;
    let chat = Arc::new(ScriptedChatClient::new(vec![no_op, no_op]));
    let dream = harness.dream_use_case(Arc::clone(&chat), StubDiscovery::empty());

    let first = dream.execute(3_600, true).await.unwrap();
    assert_eq!(first.clusters_found, 1);

    // A second cycle with nothing new still consolidates — a requested dream
    // never short-circuits.
    let second = dream.execute(3_600, true).await.unwrap();
    assert_eq!(second.clusters_found, 1);
    assert_eq!(chat.recorded_calls().await.len(), 2);

    // Both runs are recorded.
    let run = harness
        .memory_repo
        .last_dream_run()
        .await
        .unwrap()
        .expect("dream run recorded");
    assert_eq!(run.operations_applied, 0);
}

#[tokio::test]
async fn dream_rejects_deletes_outside_the_cluster() {
    let harness = Harness::new();
    harness
        .seed_item(
            MemoryKind::Fact,
            "innocent_bystander",
            "unrelated but precious",
            &test_vector(5, 0.0),
        )
        .await;
    harness
        .seed_item(
            MemoryKind::Experience,
            "dup_a",
            "first take",
            &test_vector(0, 0.05),
        )
        .await;
    harness
        .seed_item(
            MemoryKind::Experience,
            "dup_b",
            "second take",
            &test_vector(0, 0.10),
        )
        .await;

    // A misbehaving model tries to delete an item it was never shown.
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"items": [{"kind": "experience", "name": "dup_a", "content": "merged take", "project": null}],
            "delete": [{"kind": "experience", "name": "dup_b"},
                       {"kind": "fact", "name": "innocent_bystander"}]}"#,
    ]));
    let dream = harness.dream_use_case(chat, StubDiscovery::empty());
    let report = dream.execute(3_600, true).await.unwrap();

    // In-cluster delete applied; out-of-cluster delete refused.
    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Fact, "innocent_bystander")
        .await
        .unwrap()
        .is_some());
    assert!(report
        .skipped
        .iter()
        .any(|(op, reason)| matches!(op, MemoryOperation::Delete { name, .. } if name == "innocent_bystander")
            && reason.contains("not part of the examined cluster")));
}

#[tokio::test]
async fn dream_harvests_only_idle_unimported_sessions() {
    let harness = Harness::new();
    let now = now_secs();

    let mut discovery = StubDiscovery::empty();
    // One session finished two hours ago, one still active ten minutes ago.
    discovery.sessions = vec![
        discovered("old-session", now - 7_200),
        discovered("fresh-session", now - 600),
    ];
    discovery.transcripts.insert(
        "old-session".to_string(),
        transcript(
            "old-session",
            &[
                ("user", "please fix the flaky test"),
                ("assistant", "done, the race was in setup"),
            ],
        ),
    );

    // Script: one extraction call for the harvested session. No consolidation
    // call follows (a single item cannot form a cluster).
    let chat = Arc::new(ScriptedChatClient::new(vec![&extraction_json((
        "flaky_test_fix",
        "Races in test setup cause flakiness",
    ))]));
    let dream = harness.dream_use_case(chat, discovery);
    let report = dream.execute(3_600, true).await.unwrap();

    assert_eq!(report.sessions_eligible, 1, "fresh session is not eligible");
    assert_eq!(report.sessions_imported, 1);
    assert!(harness
        .memory_repo
        .find_session("old-session")
        .await
        .unwrap()
        .is_some());
    assert!(harness
        .memory_repo
        .find_session("fresh-session")
        .await
        .unwrap()
        .is_none());
    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Preference, "flaky_test_fix")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn dream_skips_harvest_when_auto_import_off() {
    // With auto-import off, a dream cycle must import no sessions — it only
    // consolidates/reflects over the existing store. This is what makes the
    // `auto_import: false` toggle mean "never import automatically", even while
    // dreaming is enabled.
    let harness = Harness::new();
    let now = now_secs();

    let mut discovery = StubDiscovery::empty();
    discovery.sessions = vec![discovered("old-session", now - 7_200)];
    discovery.transcripts.insert(
        "old-session".to_string(),
        transcript(
            "old-session",
            &[
                ("user", "please fix the flaky test"),
                ("assistant", "done, the race was in setup"),
            ],
        ),
    );

    // No LLM calls are expected: harvest is skipped, and an empty store has
    // nothing to consolidate or reflect on. An empty script asserts that.
    let chat = Arc::new(ScriptedChatClient::new(vec![]));
    let dream = harness.dream_use_case(chat, discovery);
    let report = dream.execute(3_600, false).await.unwrap();

    assert_eq!(report.sessions_eligible, 0, "harvest phase was skipped");
    assert_eq!(report.sessions_imported, 0);
    assert!(
        harness
            .memory_repo
            .find_session("old-session")
            .await
            .unwrap()
            .is_none(),
        "no session should be imported with auto-import off"
    );
}

#[tokio::test]
async fn dream_reflection_writes_but_never_deletes() {
    let harness = Harness::new();
    // Four items on orthogonal axes: no clusters, but enough for reflection.
    for (i, name) in ["exp_one", "exp_two", "exp_three", "exp_four"]
        .iter()
        .enumerate()
    {
        harness
            .seed_item(
                MemoryKind::Experience,
                name,
                "ran the migration checklist before deploying",
                &test_vector(i * 3, 0.0),
            )
            .await;
    }

    // Reflection promotes the repeated experiences to a skill, and (illegally)
    // tries to delete one of them.
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"items": [{"kind": "skill", "name": "migration_checklist",
            "content": "Before deploying: run the migration checklist.", "project": null}],
            "delete": [{"kind": "experience", "name": "exp_one"}]}"#,
    ]));
    let dream = harness.dream_use_case(chat, StubDiscovery::empty());
    let report = dream.execute(3_600, true).await.unwrap();

    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Skill, "migration_checklist")
        .await
        .unwrap()
        .is_some());
    assert!(
        harness
            .memory_repo
            .find_item(MemoryKind::Experience, "exp_one")
            .await
            .unwrap()
            .is_some(),
        "reflection must not delete"
    );
    assert!(report
        .skipped
        .iter()
        .any(|(op, _)| matches!(op, MemoryOperation::Delete { name, .. } if name == "exp_one")));
}

#[tokio::test]
async fn dream_synthesizes_skills_from_recurring_experiences() {
    let harness = Harness::new();
    // Three experiences on orthogonal axes: no clusters (so consolidation makes
    // no LLM call), but enough procedural items to trigger skill synthesis.
    for (i, name) in ["debug_flaky_one", "debug_flaky_two", "debug_flaky_three"]
        .iter()
        .enumerate()
    {
        harness
            .seed_item(
                MemoryKind::Experience,
                name,
                "reproduced the flaky test, added a barrier, verified",
                &test_vector(i * 3, 0.0),
            )
            .await;
    }

    // With three items, reflection is skipped (its floor is higher), so skill
    // synthesis is the only dream LLM call. It distills a reusable skill, plus
    // a non-skill item that must be dropped and an illegal delete that must be
    // rejected — synthesis is write-only.
    let chat = Arc::new(ScriptedChatClient::new(vec![
        r#"{"items": [
            {"kind": "skill", "name": "fix_flaky_test",
             "content": "When to use: a test fails intermittently. Steps: reproduce, add a barrier, verify.",
             "project": null},
            {"kind": "fact", "name": "not_a_skill", "content": "should be dropped", "project": null}
        ], "delete": [{"kind": "experience", "name": "debug_flaky_one"}]}"#,
    ]));
    let dream = harness.dream_use_case(chat, StubDiscovery::empty());
    let report = dream.execute(3_600, true).await.unwrap();

    // The skill was written.
    assert!(
        harness
            .memory_repo
            .find_item(MemoryKind::Skill, "fix_flaky_test")
            .await
            .unwrap()
            .is_some(),
        "recurring procedure should be distilled into a skill"
    );
    // The non-skill item proposed by synthesis was dropped, not written.
    assert!(harness
        .memory_repo
        .find_item(MemoryKind::Fact, "not_a_skill")
        .await
        .unwrap()
        .is_none());
    // Synthesis is write-only: the source experience survives.
    assert!(
        harness
            .memory_repo
            .find_item(MemoryKind::Experience, "debug_flaky_one")
            .await
            .unwrap()
            .is_some(),
        "skill synthesis must not delete"
    );
    assert!(report
        .applied
        .iter()
        .any(|op| matches!(op, MemoryOperation::Upsert { name, .. } if name == "fix_flaky_test")));
}

#[tokio::test]
async fn dream_run_round_trips_through_repository() {
    let harness = Harness::new();
    let run = codesearch::DreamRun {
        id: "run-1".to_string(),
        started_at: 10,
        finished_at: 20,
        sessions_imported: 3,
        clusters_found: 2,
        operations_applied: 5,
        operations_skipped: 1,
        status: "completed".to_string(),
    };
    harness.memory_repo.record_dream_run(&run).await.unwrap();
    let loaded = harness.memory_repo.last_dream_run().await.unwrap().unwrap();
    assert_eq!(loaded.id, "run-1");
    assert_eq!(loaded.sessions_imported, 3);
    assert_eq!(loaded.operations_applied, 5);
    assert_eq!(loaded.status, "completed");
}

// ---------------------------------------------------------------------------
// Project-scoped memory: retrieval filtering + per-project digests
// ---------------------------------------------------------------------------

/// Seed one item (optionally project-specific) with the mock embedding of its content.
async fn seed_project_item(
    repo: &Arc<dyn MemoryRepository>,
    embedding: &Arc<dyn EmbeddingService>,
    name: &str,
    content: &str,
    project: Option<&str>,
) {
    let item = MemoryItem::new(
        format!("id-{name}"),
        MemoryKind::Fact,
        name.to_string(),
        content.to_string(),
        None,
        project.map(str::to_string),
        100,
        100,
        0,
    );
    let vector = embedding.embed_query(content).await.unwrap();
    repo.upsert_item(&item, Some(&vector)).await.unwrap();
}

#[tokio::test]
async fn project_filter_returns_project_plus_global_items() {
    let harness = Harness::new();
    // Identical content so every item matches the query equally; only the
    // project filter separates them.
    let content = "the service uses postgres for persistence";
    seed_project_item(
        &harness.memory_repo,
        &harness.embedding,
        "global_fact",
        content,
        None,
    )
    .await;
    seed_project_item(
        &harness.memory_repo,
        &harness.embedding,
        "alpha_fact",
        content,
        Some("alpha"),
    )
    .await;
    seed_project_item(
        &harness.memory_repo,
        &harness.embedding,
        "beta_fact",
        content,
        Some("beta"),
    )
    .await;

    let search = MemorySearchUseCase::new(
        Arc::clone(&harness.memory_repo),
        Arc::clone(&harness.embedding),
    );

    // Unfiltered: everything.
    let all = search.execute("postgres", None, None, 10).await.unwrap();
    assert_eq!(all.len(), 3);

    // Scoped: that project's items plus globals, never the other project's.
    let alpha = search
        .execute("postgres", None, Some("alpha"), 10)
        .await
        .unwrap();
    let names: Vec<&str> = alpha.iter().map(|(i, _)| i.name()).collect();
    assert_eq!(alpha.len(), 2, "expected global + alpha, got {names:?}");
    assert!(names.contains(&"global_fact"));
    assert!(names.contains(&"alpha_fact"));

    // Keyword-only search honours the filter too.
    let repo_kw: Arc<dyn MemoryRepository> = Arc::clone(&harness.memory_repo);
    let kw = repo_kw
        .search_keyword("postgres", None, Some("beta"), 10)
        .await
        .unwrap();
    let kw_names: Vec<&str> = kw.iter().map(|(i, _)| i.name()).collect();
    assert_eq!(kw.len(), 2, "expected global + beta, got {kw_names:?}");
    assert!(kw_names.contains(&"beta_fact"));
}

#[tokio::test]
async fn project_digests_track_projects_and_remove_stale_ones() {
    let harness = Harness::new();
    let chat = Arc::new(ScriptedChatClient::new(vec![]));
    let summary = SummarizeMemoryUseCase::new(
        chat as Arc<dyn ChatClient>,
        Arc::clone(&harness.memory_repo),
        Arc::clone(&harness.embedding),
    );

    seed_project_item(
        &harness.memory_repo,
        &harness.embedding,
        "alpha_style",
        "uses tabs",
        Some("alpha"),
    )
    .await;
    seed_project_item(
        &harness.memory_repo,
        &harness.embedding,
        "alpha_ci",
        "ci is jenkins",
        Some("alpha"),
    )
    .await;
    seed_project_item(
        &harness.memory_repo,
        &harness.embedding,
        "beta_db",
        "uses sqlite",
        Some("beta"),
    )
    .await;
    seed_project_item(
        &harness.memory_repo,
        &harness.embedding,
        "global_editor",
        "prefers vim",
        None,
    )
    .await;

    // One digest per project; the global item contributes to neither. Digest
    // URIs carry a hash suffix (project names are not injective through the
    // slug), so match project digests by content rather than by exact URI.
    let regenerated = summary.regenerate_project_digests().await.unwrap();
    assert_eq!(regenerated, 2);

    let project_digest = |needle: &'static str| {
        let repo = Arc::clone(&harness.memory_repo);
        async move {
            repo.list_nodes(Some(NodeKind::Project))
                .await
                .unwrap()
                .into_iter()
                .find(|n| n.uri().contains(needle))
        }
    };

    let alpha = project_digest("alpha")
        .await
        .expect("alpha digest should exist");
    assert_eq!(alpha.kind(), NodeKind::Project);
    assert!(!alpha.abstract_().is_empty());
    // The digest carries the original project string as its label (the URI
    // slugifies it), and it round-trips through DuckDB.
    assert_eq!(alpha.label(), Some("alpha"));

    // Beta had a single item: written via the deterministic fallback.
    let beta = project_digest("beta")
        .await
        .expect("beta digest should exist");
    assert!(beta.overview().contains("beta_db"));
    assert_eq!(beta.label(), Some("beta"));

    // Nothing changed since: a second pass regenerates nothing.
    assert_eq!(summary.regenerate_project_digests().await.unwrap(), 0);

    // Remove beta's only item: its digest disappears, alpha's stays.
    harness
        .memory_repo
        .delete_item(MemoryKind::Fact, "beta_db")
        .await
        .unwrap();
    summary.regenerate_project_digests().await.unwrap();
    assert!(project_digest("beta").await.is_none());
    assert!(project_digest("alpha").await.is_some());
}

#[tokio::test]
async fn memory_project_prefers_indexed_namespace_over_directory_name() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo_root = dir.path().join("myrepo");
    std::fs::create_dir(&repo_root).unwrap();
    let canonical = std::fs::canonicalize(&repo_root).unwrap();
    let db_path = dir.path().join("codesearch.duckdb");

    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE repositories (
                id TEXT, name TEXT, path TEXT, namespace TEXT,
                git_remote TEXT, updated_at BIGINT
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repositories VALUES ('id-1', 'myrepo', ?1, 'teamns', NULL, 1)",
            duckdb::params![canonical.to_string_lossy()],
        )
        .unwrap();
    }

    let cwd = canonical.to_string_lossy().into_owned();
    // Indexed under a user-created namespace: sessions share its project.
    assert_eq!(
        codesearch::resolve_memory_project(Some(&db_path), &cwd),
        Some("teamns".to_string())
    );

    // Indexed under the default namespace, no remote: nothing stable to key on,
    // so the session is global rather than scoped to a throwaway directory name.
    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        conn.execute("UPDATE repositories SET namespace = 'search'", [])
            .unwrap();
    }
    assert_eq!(
        codesearch::resolve_memory_project(Some(&db_path), &cwd),
        None
    );

    // Not indexed at all, no remote, nothing indexed along the path: global.
    let other = dir.path().join("otherproj");
    std::fs::create_dir(&other).unwrap();
    assert_eq!(
        codesearch::resolve_memory_project(Some(&db_path), &other.to_string_lossy()),
        None
    );
}

/// A session run in a directory that *contains* indexed repos — all in one
/// user-created namespace — is attributed to that namespace, even though the
/// directory itself is not a git repo and is not indexed.
#[tokio::test]
async fn memory_project_infers_namespace_from_contained_repos() {
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let repo_a = workspace.join("svc-a");
    let repo_b = workspace.join("svc-b");
    std::fs::create_dir(&repo_a).unwrap();
    std::fs::create_dir(&repo_b).unwrap();
    let ws = std::fs::canonicalize(&workspace).unwrap();
    let pa = std::fs::canonicalize(&repo_a).unwrap();
    let pb = std::fs::canonicalize(&repo_b).unwrap();
    let db_path = dir.path().join("codesearch.duckdb");

    let seed = |extra: &str| {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS repositories (
                id TEXT, name TEXT, path TEXT, namespace TEXT,
                git_remote TEXT, updated_at BIGINT
            )",
        )
        .unwrap();
        conn.execute_batch(extra).unwrap();
    };
    seed(&format!(
        "INSERT INTO repositories VALUES \
         ('a', 'svc-a', '{}', 'backend', NULL, 1), \
         ('b', 'svc-b', '{}', 'backend', NULL, 1);",
        pa.to_string_lossy(),
        pb.to_string_lossy(),
    ));

    // Running in the workspace root (not itself a repo) infers the shared ns.
    assert_eq!(
        codesearch::resolve_memory_project(Some(&db_path), &ws.to_string_lossy()),
        Some("backend".to_string())
    );

    // A conflict — a second repo under the workspace in a DIFFERENT namespace —
    // is ambiguous, so nothing is inferred and the session stays global.
    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        // Path is built under the canonical workspace so the prefix match fires;
        // it need not exist on disk for the query, the row is enough.
        conn.execute(
            "INSERT INTO repositories VALUES ('c', 'svc-c', ?1, 'frontend', NULL, 1)",
            duckdb::params![ws.join("svc-c").to_string_lossy()],
        )
        .unwrap();
    }
    assert_eq!(
        codesearch::resolve_memory_project(Some(&db_path), &ws.to_string_lossy()),
        None
    );
}

/// A session run in a subfolder *inside* an indexed repo (with no remote and no
/// direct row for that subfolder) is attributed to the enclosing repo's
/// namespace — inference looks upward as well as downward.
#[tokio::test]
async fn memory_project_infers_namespace_from_enclosing_repo() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo_root = dir.path().join("svc-a");
    let nested = repo_root.join("src").join("inner");
    std::fs::create_dir_all(&nested).unwrap();
    let pa = std::fs::canonicalize(&repo_root).unwrap();
    let cwd = std::fs::canonicalize(&nested).unwrap();
    let db_path = dir.path().join("codesearch.duckdb");

    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE repositories (
                id TEXT, name TEXT, path TEXT, namespace TEXT,
                git_remote TEXT, updated_at BIGINT
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repositories VALUES ('a', 'svc-a', ?1, 'backend', NULL, 1)",
            duckdb::params![pa.to_string_lossy()],
        )
        .unwrap();
    }

    assert_eq!(
        codesearch::resolve_memory_project(Some(&db_path), &cwd.to_string_lossy()),
        Some("backend".to_string())
    );
}

/// A repo with a git remote keeps the SAME memory project whether or not it has
/// been indexed, so memories written before indexing still match sessions run
/// after — they are not orphaned under a directory name that stops being used.
#[tokio::test]
async fn memory_project_uses_stable_remote_when_not_yet_indexed() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo_root = dir.path().join("myrepo");
    std::fs::create_dir(&repo_root).unwrap();
    // Give it a git remote (no network — just `.git/config`).
    let git = repo_root.join(".git");
    std::fs::create_dir(&git).unwrap();
    std::fs::write(
        git.join("config"),
        "[remote \"origin\"]\n\turl = git@github.com:owner/repo.git\n",
    )
    .unwrap();

    let canonical = std::fs::canonicalize(&repo_root).unwrap();
    let cwd = canonical.to_string_lossy().into_owned();
    let db_path = dir.path().join("codesearch.duckdb");

    // Not indexed at all: the remote is the project, not the directory name.
    let before = codesearch::resolve_memory_project(Some(&db_path), &cwd);
    assert_eq!(before.as_deref(), Some("github.com/owner/repo"));

    // Later indexed under the default namespace: the project is unchanged, so
    // pre-index memories still line up with post-index sessions.
    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE repositories (
                id TEXT, name TEXT, path TEXT, namespace TEXT,
                git_remote TEXT, updated_at BIGINT
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repositories VALUES ('id-1', 'myrepo', ?1, 'search', ?2, 1)",
            duckdb::params![cwd, "github.com/owner/repo"],
        )
        .unwrap();
    }
    let after = codesearch::resolve_memory_project(Some(&db_path), &cwd);
    assert_eq!(after, before, "remote-keyed project must survive indexing");
}
