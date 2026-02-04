use async_trait::async_trait;

use crate::domain::{DomainError, SymbolReference};

/// Query options for call graph lookups.
#[derive(Debug, Clone, Default)]
pub struct CallGraphQuery {
    /// Filter by repository ID
    pub repository_id: Option<String>,
    /// Filter by language
    pub language: Option<String>,
    /// Filter by reference kind
    pub reference_kind: Option<String>,
    /// Maximum number of results to return
    pub limit: Option<u32>,
}

impl CallGraphQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_repository(mut self, repository_id: impl Into<String>) -> Self {
        self.repository_id = Some(repository_id.into());
        self
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    pub fn with_reference_kind(mut self, kind: impl Into<String>) -> Self {
        self.reference_kind = Some(kind.into());
        self
    }

    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Persistence for symbol references (call graph edges).
#[async_trait]
pub trait CallGraphRepository: Send + Sync {
    /// Save a batch of symbol references.
    async fn save_batch(&self, references: &[SymbolReference]) -> Result<(), DomainError>;

    /// Find all references where the given symbol is the callee (what calls this symbol?).
    async fn find_callers(
        &self,
        callee_symbol: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError>;

    /// Find all references where the given symbol is the caller (what does this symbol call?).
    async fn find_callees(
        &self,
        caller_symbol: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError>;

    /// Find all references in a specific file.
    async fn find_by_file(
        &self,
        file_path: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError>;

    /// Find all references for a specific repository.
    async fn find_by_repository(
        &self,
        repository_id: &str,
    ) -> Result<Vec<SymbolReference>, DomainError>;

    /// Delete all references for a specific file within a repository.
    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError>;

    /// Delete all references for a repository.
    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;

    /// Get statistics about the call graph for a repository.
    async fn get_stats(&self, repository_id: &str) -> Result<CallGraphStats, DomainError>;

    /// Find symbols that reference a given symbol across all repositories.
    /// Useful for cross-project analysis.
    async fn find_cross_repo_references(
        &self,
        symbol_name: &str,
    ) -> Result<Vec<SymbolReference>, DomainError>;
}

/// Statistics about the call graph for a repository.
#[derive(Debug, Clone, Default)]
pub struct CallGraphStats {
    /// Total number of symbol references
    pub total_references: u64,
    /// Number of unique caller symbols
    pub unique_callers: u64,
    /// Number of unique callee symbols
    pub unique_callees: u64,
    /// Breakdown by reference kind
    pub by_reference_kind: Vec<(String, u64)>,
    /// Breakdown by language
    pub by_language: Vec<(String, u64)>,
}
