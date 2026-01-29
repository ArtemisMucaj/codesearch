use async_trait::async_trait;

use crate::domain::{CodeChunk, DomainError, Language, NodeType};

/// Repository trait for code chunk persistence.
#[async_trait]
pub trait ChunkRepository: Send + Sync {
    async fn save(&self, chunk: &CodeChunk) -> Result<(), DomainError>;
    async fn save_batch(&self, chunks: &[CodeChunk]) -> Result<(), DomainError>;
    async fn find_by_id(&self, id: &str) -> Result<Option<CodeChunk>, DomainError>;
    async fn find_by_file(&self, file_path: &str) -> Result<Vec<CodeChunk>, DomainError>;
    async fn find_by_repository(&self, repository_id: &str) -> Result<Vec<CodeChunk>, DomainError>;
    async fn find_by_language(&self, language: Language) -> Result<Vec<CodeChunk>, DomainError>;
    async fn find_by_node_type(&self, node_type: NodeType) -> Result<Vec<CodeChunk>, DomainError>;
    async fn delete(&self, id: &str) -> Result<(), DomainError>;
    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;
    async fn delete_by_file(&self, file_path: &str) -> Result<(), DomainError>;
    async fn count(&self) -> Result<u64, DomainError>;
    async fn count_by_repository(&self, repository_id: &str) -> Result<u64, DomainError>;
}
