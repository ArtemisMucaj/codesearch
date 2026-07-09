use async_trait::async_trait;

use crate::domain::{DomainError, ImportedSession, MemoryItem, MemoryKind};

/// Persistence port for long-term memory items and imported-session records.
///
/// Memory lives in its own store (a dedicated DuckDB file, separate from the
/// code index) so that importing sessions never contends with indexing and
/// the memory database can be inspected, backed up, or wiped independently.
#[async_trait]
pub trait MemoryRepository: Send + Sync {
    /// Insert or replace a memory item, keyed by `(kind, name)`.
    ///
    /// `vector` is the embedding of the item content; `None` when embeddings
    /// are unavailable (the item remains keyword-searchable).
    async fn upsert_item(
        &self,
        item: &MemoryItem,
        vector: Option<&[f32]>,
    ) -> Result<(), DomainError>;

    async fn find_item(
        &self,
        kind: MemoryKind,
        name: &str,
    ) -> Result<Option<MemoryItem>, DomainError>;

    /// Delete by `(kind, name)`. Returns `true` when an item was removed.
    async fn delete_item(&self, kind: MemoryKind, name: &str) -> Result<bool, DomainError>;

    /// Delete by item ID. Returns `true` when an item was removed.
    async fn delete_item_by_id(&self, id: &str) -> Result<bool, DomainError>;

    /// List items, optionally restricted to one kind, newest first.
    async fn list_items(&self, kind: Option<MemoryKind>) -> Result<Vec<MemoryItem>, DomainError>;

    /// Cosine-similarity search over item embeddings.
    /// Returns `(item, score)` pairs, best first, score in `[0, 1]`.
    async fn search_semantic(
        &self,
        vector: &[f32],
        kind: Option<MemoryKind>,
        limit: usize,
    ) -> Result<Vec<(MemoryItem, f32)>, DomainError>;

    /// Case-insensitive keyword search over item names and content.
    /// Returns `(item, score)` pairs, best first.
    async fn search_keyword(
        &self,
        query: &str,
        kind: Option<MemoryKind>,
        limit: usize,
    ) -> Result<Vec<(MemoryItem, f32)>, DomainError>;

    /// Record that a session has been imported (idempotence marker).
    async fn record_session(&self, session: &ImportedSession) -> Result<(), DomainError>;

    async fn find_session(&self, id: &str) -> Result<Option<ImportedSession>, DomainError>;

    /// List imported sessions, newest first.
    async fn list_sessions(&self) -> Result<Vec<ImportedSession>, DomainError>;
}
