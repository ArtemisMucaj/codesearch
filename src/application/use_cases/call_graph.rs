use std::sync::Arc;

use anyhow::Context;
use tracing::debug;

use crate::application::{CallGraphQuery, CallGraphRepository, CallGraphStats};
use crate::domain::{DomainError, SymbolReference};

/// Use case for managing call graph (symbol references).
/// Provides a decoupled interface for saving, querying, and deleting
/// symbol references populated by the SCIP indexing phase.
pub struct CallGraphUseCase {
    repository: Arc<dyn CallGraphRepository>,
}

impl CallGraphUseCase {
    pub fn new(repository: Arc<dyn CallGraphRepository>) -> Self {
        Self { repository }
    }

    /// Persist a slice of pre-extracted [`SymbolReference`]s produced by the
    /// SCIP importer.
    ///
    /// Returns the number of references saved.
    pub async fn save_references(&self, references: &[SymbolReference]) -> anyhow::Result<u64> {
        self.persist_references(references, "<scip>").await
    }

    /// Save a batch of already-extracted references. Handles the empty-slice short-circuit,
    /// the `save_batch` call with anyhow context, and the success debug log.
    async fn persist_references(
        &self,
        references: &[SymbolReference],
        file_path: &str,
    ) -> anyhow::Result<u64> {
        if references.is_empty() {
            return Ok(0);
        }

        let count = references.len() as u64;
        self.repository
            .save_batch(references)
            .await
            .with_context(|| format!("failed to save {} references for indexing", count))?;

        debug!("Saved {} references from {}", count, file_path);
        Ok(count)
    }

    /// Delete all symbol references for a specific file within a repository.
    /// Returns the number of references deleted.
    pub async fn delete_by_file(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError> {
        self.repository
            .delete_by_file_path(repository_id, file_path)
            .await
    }

    /// Delete all symbol references for a repository.
    pub async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        self.repository.delete_by_repository(repository_id).await
    }

    /// Find all references where the given symbol is the callee (what calls this symbol?).
    pub async fn find_callers(
        &self,
        callee_symbol: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        self.repository.find_callers(callee_symbol, query).await
    }

    /// Find all references where the given symbol is the caller (what does this symbol call?).
    pub async fn find_callees(
        &self,
        caller_symbol: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        self.repository.find_callees(caller_symbol, query).await
    }

    /// Find all references in a specific file.
    pub async fn find_by_file(
        &self,
        file_path: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        self.repository.find_by_file(file_path, query).await
    }

    /// Find all references for a specific repository.
    pub async fn find_by_repository(
        &self,
        repository_id: &str,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        self.repository.find_by_repository(repository_id).await
    }

    /// Get statistics about the call graph for a repository.
    pub async fn get_stats(&self, repository_id: &str) -> Result<CallGraphStats, DomainError> {
        self.repository.get_stats(repository_id).await
    }

    /// Find symbols that reference a given symbol across all repositories.
    pub async fn find_cross_repo_references(
        &self,
        symbol_name: &str,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        self.repository
            .find_cross_repo_references(symbol_name)
            .await
    }
}
