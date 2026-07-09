//! Integration tests for the session-memory pipeline:
//! transcript parsing → LLM extraction (scripted) → DuckDB storage → search.
//!
//! Uses an in-memory memory database, mock embeddings, and a scripted chat
//! client, so no network or model download is required.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use codesearch::{
    parse_transcript, ChatClient, DomainError, DuckdbMemoryRepository, EmbeddingService,
    ImportOutcome, ImportSessionUseCase, MemoryExtractionUseCase, MemoryKind, MemoryRepository,
    MemorySearchUseCase, MockEmbedding, NoEmbedding, SessionMessage, SessionTranscript,
};

/// Chat client that replays a fixed sequence of responses.
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

    async fn recorded_calls(&self) -> Vec<(String, String)> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl ChatClient for ScriptedChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
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
            chat,
            Arc::clone(&self.memory_repo),
            Arc::clone(&self.embedding),
        );
        ImportSessionUseCase::new(Arc::clone(&self.memory_repo), extraction)
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
    let extraction =
        MemoryExtractionUseCase::new(chat, Arc::clone(&memory_repo), Arc::clone(&embedding));
    let use_case = ImportSessionUseCase::new(Arc::clone(&memory_repo), extraction);

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
