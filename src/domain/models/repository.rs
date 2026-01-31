use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

/// The type of vector storage backend used for a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VectorStore {
    #[default]
    DuckDb,
    ChromaDb,
    InMemory,
}

impl VectorStore {
    pub fn as_str(&self) -> &'static str {
        match self {
            VectorStore::DuckDb => "duckdb",
            VectorStore::ChromaDb => "chromadb",
            VectorStore::InMemory => "memory",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "duckdb" => VectorStore::DuckDb,
            "chromadb" | "chroma" => VectorStore::ChromaDb,
            "memory" | "inmemory" | "in_memory" => VectorStore::InMemory,
            unknown => {
                warn!(
                    "Unknown vector store type '{}', defaulting to DuckDB",
                    unknown
                );
                VectorStore::DuckDb
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    id: String,
    name: String,
    path: String,
    created_at: i64,
    updated_at: i64,
    chunk_count: u64,
    file_count: u64,
    /// Vector store backend (duckdb, chromadb, memory).
    store: VectorStore,
    /// Namespace for vector storage (DuckDB schema or ChromaDB collection).
    namespace: Option<String>,
}

impl Repository {
    pub fn new(name: String, path: String) -> Self {
        let now = current_timestamp();
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            path,
            created_at: now,
            updated_at: now,
            chunk_count: 0,
            file_count: 0,
            store: VectorStore::default(),
            namespace: None,
        }
    }

    pub fn new_with_storage(
        name: String,
        path: String,
        store: VectorStore,
        namespace: Option<String>,
    ) -> Self {
        let now = current_timestamp();
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            path,
            created_at: now,
            updated_at: now,
            chunk_count: 0,
            file_count: 0,
            store,
            namespace,
        }
    }

    /// Reconstitutes from persisted data (used by adapters).
    pub fn reconstitute(
        id: String,
        name: String,
        path: String,
        created_at: i64,
        updated_at: i64,
        chunk_count: u64,
        file_count: u64,
        store: VectorStore,
        namespace: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            path,
            created_at,
            updated_at,
            chunk_count,
            file_count,
            store,
            namespace,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    pub fn updated_at(&self) -> i64 {
        self.updated_at
    }

    pub fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    pub fn file_count(&self) -> u64 {
        self.file_count
    }

    pub fn store(&self) -> VectorStore {
        self.store
    }

    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub fn update_stats(&mut self, chunk_count: u64, file_count: u64) {
        self.chunk_count = chunk_count;
        self.file_count = file_count;
        self.updated_at = current_timestamp();
    }

    pub fn is_indexed(&self) -> bool {
        self.chunk_count > 0
    }

    pub fn is_empty(&self) -> bool {
        self.file_count == 0
    }

    pub fn average_chunks_per_file(&self) -> f64 {
        if self.file_count == 0 {
            0.0
        } else {
            self.chunk_count as f64 / self.file_count as f64
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} ({} files, {} chunks)",
            self.name, self.file_count, self.chunk_count
        )
    }

    pub fn matches_path(&self, path: &str) -> bool {
        self.path == path
    }

    pub fn age_seconds(&self) -> i64 {
        current_timestamp().saturating_sub(self.created_at)
    }

    pub fn seconds_since_update(&self) -> i64 {
        current_timestamp().saturating_sub(self.updated_at)
    }
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Represents the current indexing status of a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexingStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl IndexingStatus {
    pub fn is_complete(&self) -> bool {
        matches!(self, IndexingStatus::Completed)
    }

    pub fn is_in_progress(&self) -> bool {
        matches!(self, IndexingStatus::InProgress)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, IndexingStatus::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repository_creation() {
        let repo = Repository::new("my-repo".to_string(), "/path/to/repo".to_string());

        assert_eq!(repo.name(), "my-repo");
        assert_eq!(repo.path(), "/path/to/repo");
        assert_eq!(repo.chunk_count(), 0);
        assert_eq!(repo.file_count(), 0);
        assert!(!repo.is_indexed());
        assert!(repo.is_empty());
    }

    #[test]
    fn test_update_stats() {
        let mut repo = Repository::new("test".to_string(), "/test".to_string());

        repo.update_stats(100, 10);

        assert_eq!(repo.chunk_count(), 100);
        assert_eq!(repo.file_count(), 10);
        assert!(repo.is_indexed());
        assert!(!repo.is_empty());
    }

    #[test]
    fn test_average_chunks_per_file() {
        let mut repo = Repository::new("test".to_string(), "/test".to_string());

        repo.update_stats(50, 10);

        assert!((repo.average_chunks_per_file() - 5.0).abs() < 0.01);
    }

    #[test]
    fn test_empty_repo_average() {
        let repo = Repository::new("test".to_string(), "/test".to_string());

        assert_eq!(repo.average_chunks_per_file(), 0.0);
    }
}
