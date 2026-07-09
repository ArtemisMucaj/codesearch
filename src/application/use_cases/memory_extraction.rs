//! Session memory extraction — the codesearch port of OpenViking's
//! `ExtractLoop`.
//!
//! Flow (single LLM call with one format-recovery retry, mirroring the
//! simplified ReAct orchestrator in OpenViking):
//!
//! 1. **Prefetch** — embed a compact query built from the transcript and
//!    fetch the most similar existing memories, so the model can merge new
//!    information into them instead of creating duplicates.
//! 2. **Extract** — send the extraction instruction + memory-kind schemas +
//!    existing memories + conversation to the (small) chat model, which
//!    returns one JSON object of upsert/delete operations.
//! 3. **Apply** — validate and normalize the operations, then write them to
//!    the memory repository with fresh embeddings.

use std::sync::Arc;

use serde::Deserialize;
use tracing::{debug, warn};

use crate::application::interfaces::{ChatClient, EmbeddingService, MemoryRepository};
use crate::application::use_cases::memory_extraction_prompt as prompt;
use crate::domain::{DomainError, MemoryItem, MemoryKind, MemoryOperation, SessionTranscript};

/// How many existing memories are prefetched into the extraction context.
const PREFETCH_LIMIT: usize = 8;

/// Upper bound on operations applied from a single extraction, as a guard
/// against a runaway model flooding the store with noise.
const MAX_OPERATIONS_PER_RUN: usize = 24;

/// Maximum length of a normalized item name.
const MAX_NAME_CHARS: usize = 64;

/// Outcome of one extraction run.
#[derive(Debug, Default)]
pub struct ExtractionReport {
    /// Operations that were applied, in order.
    pub applied: Vec<MemoryOperation>,
    /// Operations that were skipped, with the reason.
    pub skipped: Vec<(MemoryOperation, String)>,
}

impl ExtractionReport {
    pub fn items_written(&self) -> usize {
        self.applied
            .iter()
            .filter(|op| matches!(op, MemoryOperation::Upsert { .. }))
            .count()
    }
}

/// JSON shape the extraction model must return.
#[derive(Debug, Deserialize)]
struct ExtractionOutput {
    #[serde(default)]
    preferences: Vec<RawItem>,
    #[serde(default)]
    experiences: Vec<RawItem>,
    #[serde(default)]
    skills: Vec<RawItem>,
    #[serde(default)]
    facts: Vec<RawItem>,
    #[serde(default)]
    delete: Vec<RawDelete>,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct RawDelete {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
}

pub struct MemoryExtractionUseCase {
    chat_client: Arc<dyn ChatClient>,
    memory_repo: Arc<dyn MemoryRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
}

impl MemoryExtractionUseCase {
    pub fn new(
        chat_client: Arc<dyn ChatClient>,
        memory_repo: Arc<dyn MemoryRepository>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            chat_client,
            memory_repo,
            embedding_service,
        }
    }

    /// Run extraction over a transcript and apply the resulting operations.
    #[tracing::instrument(skip_all, fields(session_id = %transcript.id))]
    pub async fn execute(
        &self,
        transcript: &SessionTranscript,
    ) -> Result<ExtractionReport, DomainError> {
        let existing = self.prefetch(transcript).await;
        let operations = self.extract(transcript, &existing).await?;
        self.apply(transcript, operations).await
    }

    /// Fetch existing memories related to this conversation so the model can
    /// update them in place. Prefetch failures degrade to "no context" rather
    /// than failing the import.
    async fn prefetch(&self, transcript: &SessionTranscript) -> Vec<MemoryItem> {
        if self.embedding_service.embeddings_enabled() {
            let query = prompt::prefetch_query(transcript);
            if query.is_empty() {
                return Vec::new();
            }
            match self.embedding_service.embed_query(&query).await {
                Ok(vector) => {
                    match self
                        .memory_repo
                        .search_semantic(&vector, None, PREFETCH_LIMIT)
                        .await
                    {
                        Ok(results) => return results.into_iter().map(|(item, _)| item).collect(),
                        Err(e) => warn!("memory prefetch search failed: {e}"),
                    }
                }
                Err(e) => warn!("memory prefetch embedding failed: {e}"),
            }
        }
        // No embeddings: surface the most recent items instead so merging
        // still has a chance to happen.
        match self.memory_repo.list_items(None).await {
            Ok(mut items) => {
                items.truncate(PREFETCH_LIMIT);
                items
            }
            Err(e) => {
                warn!("memory prefetch list failed: {e}");
                Vec::new()
            }
        }
    }

    /// Call the extraction model and parse its JSON output, retrying once
    /// with a format-correction message when parsing fails.
    async fn extract(
        &self,
        transcript: &SessionTranscript,
        existing: &[MemoryItem],
    ) -> Result<Vec<MemoryOperation>, DomainError> {
        let system = prompt::system_prompt();
        let user = prompt::user_prompt(transcript, existing);

        let response = self.chat_client.complete(&system, &user).await?;
        match parse_operations(&response) {
            Ok(ops) => Ok(ops),
            Err(first_err) => {
                debug!("extraction output unparseable, retrying once: {first_err}");
                let retry_user = format!("{user}\n\n{}", prompt::format_retry_prompt());
                let response = self.chat_client.complete(&system, &retry_user).await?;
                parse_operations(&response).map_err(|e| {
                    DomainError::parse(format!(
                        "extraction model returned unparseable output twice: {e}"
                    ))
                })
            }
        }
    }

    /// Apply validated operations to the memory store.
    async fn apply(
        &self,
        transcript: &SessionTranscript,
        operations: Vec<MemoryOperation>,
    ) -> Result<ExtractionReport, DomainError> {
        let mut report = ExtractionReport::default();
        let now = unix_now();

        for op in operations.into_iter() {
            if report.applied.len() >= MAX_OPERATIONS_PER_RUN {
                report
                    .skipped
                    .push((op, "operation limit reached".to_string()));
                continue;
            }
            match op {
                MemoryOperation::Upsert {
                    kind,
                    ref name,
                    ref content,
                } => {
                    let existing = self.memory_repo.find_item(kind, name).await?;
                    let item = match existing {
                        Some(prev) => MemoryItem::new(
                            prev.id().to_string(),
                            kind,
                            name.clone(),
                            content.clone(),
                            Some(transcript.id.clone()),
                            prev.created_at(),
                            now,
                            prev.update_count() + 1,
                        ),
                        None => MemoryItem::new(
                            uuid::Uuid::new_v4().to_string(),
                            kind,
                            name.clone(),
                            content.clone(),
                            Some(transcript.id.clone()),
                            now,
                            now,
                            0,
                        ),
                    };
                    let vector = self.embed_content(&item).await;
                    self.memory_repo
                        .upsert_item(&item, vector.as_deref())
                        .await?;
                    report.applied.push(op);
                }
                MemoryOperation::Delete { kind, ref name } => {
                    if self.memory_repo.delete_item(kind, name).await? {
                        report.applied.push(op);
                    } else {
                        report.skipped.push((op, "item not found".to_string()));
                    }
                }
            }
        }
        Ok(report)
    }

    /// Embed `name + content` for semantic recall; `None` when embeddings are
    /// disabled or fail (the item stays keyword-searchable).
    async fn embed_content(&self, item: &MemoryItem) -> Option<Vec<f32>> {
        if !self.embedding_service.embeddings_enabled() {
            return None;
        }
        let text = format!("{}\n\n{}", item.name().replace('_', " "), item.content());
        match self.embedding_service.embed_query(&text).await {
            Ok(vector) => Some(vector),
            Err(e) => {
                warn!("failed to embed memory item '{}': {e}", item.name());
                None
            }
        }
    }
}

/// Parse the model's JSON response into validated, normalized operations.
///
/// Tolerates surrounding prose or a markdown fence by extracting the first
/// balanced top-level JSON object.
fn parse_operations(response: &str) -> Result<Vec<MemoryOperation>, DomainError> {
    let json = extract_json_object(response)
        .ok_or_else(|| DomainError::parse("no JSON object found in extraction output"))?;
    let output: ExtractionOutput = serde_json::from_str(json)
        .map_err(|e| DomainError::parse(format!("invalid extraction JSON: {e}")))?;

    let mut operations = Vec::new();
    let groups = [
        (MemoryKind::Preference, output.preferences),
        (MemoryKind::Experience, output.experiences),
        (MemoryKind::Skill, output.skills),
        (MemoryKind::Fact, output.facts),
    ];
    for (kind, items) in groups {
        for item in items {
            let Some(name) = normalize_name(&item.name) else {
                continue;
            };
            let content = item.content.trim();
            if content.is_empty() {
                continue;
            }
            operations.push(MemoryOperation::Upsert {
                kind,
                name,
                content: content.to_string(),
            });
        }
    }
    for del in output.delete {
        let Some(kind) = MemoryKind::parse(&del.kind) else {
            continue;
        };
        let Some(name) = normalize_name(&del.name) else {
            continue;
        };
        operations.push(MemoryOperation::Delete { kind, name });
    }
    Ok(operations)
}

/// Extract the first balanced `{ ... }` object from mixed model output.
fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + ch.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Normalize an item name to lowercase snake_case; `None` when empty.
fn normalize_name(raw: &str) -> Option<String> {
    let name: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_whitespace() || c == '-' {
                '_'
            } else {
                c
            }
        })
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let name = name.trim_matches('_').to_string();
    if name.is_empty() {
        return None;
    }
    Some(name.chars().take(MAX_NAME_CHARS).collect())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json_with_prose() {
        let response = r#"Here are the memories:
```json
{"preferences": [{"name": "Rust Style", "content": "Prefers ? over unwrap"}],
 "experiences": [], "skills": [], "facts": [],
 "delete": [{"kind": "fact", "name": "old_fact"}]}
```"#;
        let ops = parse_operations(response).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[0],
            MemoryOperation::Upsert {
                kind: MemoryKind::Preference,
                name: "rust_style".to_string(),
                content: "Prefers ? over unwrap".to_string(),
            }
        );
        assert_eq!(
            ops[1],
            MemoryOperation::Delete {
                kind: MemoryKind::Fact,
                name: "old_fact".to_string(),
            }
        );
    }

    #[test]
    fn skips_empty_names_and_content() {
        let response = r#"{"preferences": [{"name": "", "content": "x"},
            {"name": "ok", "content": "  "}], "experiences": [], "skills": [],
            "facts": [], "delete": []}"#;
        let ops = parse_operations(response).unwrap();
        assert!(ops.is_empty());
    }

    #[test]
    fn rejects_output_without_json() {
        assert!(parse_operations("I cannot help with that").is_err());
    }

    #[test]
    fn extracts_json_with_braces_inside_strings() {
        let response = r#"{"preferences": [{"name": "a", "content": "code: fn x() { y() }"}],
            "experiences": [], "skills": [], "facts": [], "delete": []}"#;
        let ops = parse_operations(response).unwrap();
        assert_eq!(ops.len(), 1);
    }
}
