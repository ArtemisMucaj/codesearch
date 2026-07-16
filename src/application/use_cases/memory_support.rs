//! Helpers shared by the memory write paths (per-session extraction and the
//! dream cycle), so the embedding recipe and the identity-preserving update
//! semantics are defined exactly once.

use tracing::warn;

use crate::application::interfaces::{EmbeddingService, MemoryRepository};
use crate::domain::{DomainError, MemoryItem, MemoryKind};

/// Embed `name + content` for semantic recall; `None` when embeddings are
/// disabled or fail (the item stays keyword-searchable).
pub(crate) async fn embed_memory_item(
    embedding_service: &dyn EmbeddingService,
    item: &MemoryItem,
) -> Option<Vec<f32>> {
    if !embedding_service.embeddings_enabled() {
        return None;
    }
    let text = format!("{}\n\n{}", item.name().replace('_', " "), item.content());
    match embedding_service.embed_query(&text).await {
        Ok(vector) => Some(vector),
        Err(e) => {
            warn!("failed to embed memory item '{}': {e}", item.name());
            None
        }
    }
}

/// Write one upsert, preserving the target's identity and history when it
/// already exists (same id, original `created_at`, bumped `update_count`).
///
/// `source_override` stamps the written item's source session; `None` keeps
/// the existing item's source (or leaves a new item unsourced).
pub(crate) async fn upsert_preserving_identity(
    memory_repo: &dyn MemoryRepository,
    embedding_service: &dyn EmbeddingService,
    kind: MemoryKind,
    name: &str,
    content: &str,
    scope: Option<String>,
    source_override: Option<&str>,
    now: i64,
) -> Result<(), DomainError> {
    let existing = memory_repo.find_item(kind, name).await?;
    let item = match existing {
        Some(prev) => MemoryItem::new(
            prev.id().to_string(),
            kind,
            name.to_string(),
            content.to_string(),
            source_override
                .or(prev.source_session_id())
                .map(str::to_string),
            scope,
            prev.created_at(),
            now,
            prev.update_count() + 1,
        ),
        None => MemoryItem::new(
            uuid::Uuid::new_v4().to_string(),
            kind,
            name.to_string(),
            content.to_string(),
            source_override.map(str::to_string),
            scope,
            now,
            now,
            0,
        ),
    };
    let vector = embed_memory_item(embedding_service, &item).await;
    memory_repo.upsert_item(&item, vector.as_deref()).await
}

/// Current Unix time in seconds.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
