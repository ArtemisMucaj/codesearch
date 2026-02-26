use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tracing::{debug, warn};

use crate::application::{CallGraphQuery, CallGraphRepository, CallGraphStats};
use crate::domain::{DomainError, Language, SymbolReference};

/// Trait for call graph extraction strategies.
/// This allows replacing the extraction method (e.g., tree-sitter, LSP, etc.)
/// without changing the use case logic.
#[async_trait::async_trait]
pub trait CallGraphExtractor: Send + Sync {
    /// Extract symbol references from source code.
    ///
    /// `exports_by_file` maps repo-relative file paths to the exported symbol names of
    /// that file.  Pass an empty map for languages that don't need cross-file resolution.
    async fn extract(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
        exports_by_file: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<SymbolReference>, DomainError>;

    /// Build an export index for JS/TS files so that `extract` can resolve
    /// relative `require()` paths to actual exported symbol names.
    ///
    /// Reads each JS/TS file under `absolute_path` / `relative_path` and returns
    /// a map of repo-relative path → exported symbol names.
    ///
    /// The default implementation returns an empty map (no pre-scan needed for
    /// languages that don't use cross-file import resolution).
    async fn build_export_index(
        &self,
        _absolute_path: &Path,
        _relative_paths: &[String],
    ) -> HashMap<String, Vec<String>> {
        HashMap::new()
    }
}

/// Use case for managing call graph (symbol references).
/// Provides a decoupled interface for extracting, saving, querying, and deleting
/// symbol references. The extraction strategy can be replaced by providing
/// a different CallGraphExtractor implementation.
pub struct CallGraphUseCase {
    extractor: Arc<dyn CallGraphExtractor>,
    repository: Arc<dyn CallGraphRepository>,
}

impl CallGraphUseCase {
    /// Create a new CallGraphUseCase with the given extractor and repository.
    pub fn new(
        extractor: Arc<dyn CallGraphExtractor>,
        repository: Arc<dyn CallGraphRepository>,
    ) -> Self {
        Self {
            extractor,
            repository,
        }
    }

    /// Build an export index for a set of files.
    ///
    /// Delegates to the extractor's pre-scan implementation; returns an empty
    /// map for extractors that don't need cross-file export resolution.
    pub async fn build_export_index(
        &self,
        absolute_path: &Path,
        relative_paths: &[String],
    ) -> HashMap<String, Vec<String>> {
        self.extractor
            .build_export_index(absolute_path, relative_paths)
            .await
    }

    /// Extract symbol references from content and save them to the repository.
    ///
    /// `exports_by_file` is used to resolve relative `require()` paths in JS/TS
    /// files to the actual exported symbol names.  Pass an empty map when no
    /// pre-scan has been performed or for languages that don't need it.
    ///
    /// Returns the number of references saved.
    pub async fn extract_and_save(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
        exports_by_file: &HashMap<String, Vec<String>>,
    ) -> anyhow::Result<u64> {
        match self
            .extractor
            .extract(content, file_path, language, repository_id, exports_by_file)
            .await
        {
            Ok(refs) => self.persist_references(refs, file_path).await,
            Err(e) => {
                warn!(
                    "Failed to extract references from {}: {} (continuing)",
                    file_path, e
                );
                Ok(0)
            }
        }
    }

    /// Save a batch of already-extracted references. Handles the empty-vec short-circuit,
    /// the `save_batch` call with anyhow context, and the success debug log.
    async fn persist_references(
        &self,
        references: Vec<SymbolReference>,
        file_path: &str,
    ) -> anyhow::Result<u64> {
        if references.is_empty() {
            return Ok(0);
        }

        let count = references.len() as u64;
        self.repository
            .save_batch(&references)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ReferenceKind;
    use std::sync::Mutex;

    struct MockExtractor {
        references: Vec<SymbolReference>,
    }

    impl MockExtractor {
        fn new(references: Vec<SymbolReference>) -> Self {
            Self { references }
        }
    }

    #[async_trait::async_trait]
    impl CallGraphExtractor for MockExtractor {
        async fn extract(
            &self,
            _content: &str,
            _file_path: &str,
            _language: Language,
            _repository_id: &str,
            _exports_by_file: &HashMap<String, Vec<String>>,
        ) -> Result<Vec<SymbolReference>, DomainError> {
            Ok(self.references.clone())
        }
    }

    struct MockCallGraphRepository {
        saved: Mutex<Vec<SymbolReference>>,
    }

    impl MockCallGraphRepository {
        fn new() -> Self {
            Self {
                saved: Mutex::new(Vec::new()),
            }
        }

        fn saved_count(&self) -> usize {
            self.saved.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl CallGraphRepository for MockCallGraphRepository {
        async fn save_batch(&self, references: &[SymbolReference]) -> Result<(), DomainError> {
            self.saved
                .lock()
                .unwrap()
                .extend(references.iter().cloned());
            Ok(())
        }

        async fn find_callers(
            &self,
            _callee_symbol: &str,
            _query: &CallGraphQuery,
        ) -> Result<Vec<SymbolReference>, DomainError> {
            Ok(Vec::new())
        }

        async fn find_callees(
            &self,
            _caller_symbol: &str,
            _query: &CallGraphQuery,
        ) -> Result<Vec<SymbolReference>, DomainError> {
            Ok(Vec::new())
        }

        async fn find_by_file(
            &self,
            _file_path: &str,
            _query: &CallGraphQuery,
        ) -> Result<Vec<SymbolReference>, DomainError> {
            Ok(Vec::new())
        }

        async fn find_by_repository(
            &self,
            _repository_id: &str,
        ) -> Result<Vec<SymbolReference>, DomainError> {
            Ok(Vec::new())
        }

        async fn delete_by_file_path(
            &self,
            _repository_id: &str,
            _file_path: &str,
        ) -> Result<u64, DomainError> {
            Ok(0)
        }

        async fn delete_by_repository(&self, _repository_id: &str) -> Result<(), DomainError> {
            Ok(())
        }

        async fn get_stats(&self, _repository_id: &str) -> Result<CallGraphStats, DomainError> {
            Ok(CallGraphStats::default())
        }

        async fn find_cross_repo_references(
            &self,
            _symbol_name: &str,
        ) -> Result<Vec<SymbolReference>, DomainError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn test_extract_and_save() {
        let references = vec![SymbolReference::new(
            Some("caller".to_string()),
            "callee".to_string(),
            "test.rs".to_string(),
            "test.rs".to_string(),
            10,
            5,
            ReferenceKind::Call,
            Language::Rust,
            "repo-1".to_string(),
        )];

        let extractor = Arc::new(MockExtractor::new(references));
        let repository = Arc::new(MockCallGraphRepository::new());

        let use_case = CallGraphUseCase::new(extractor, repository.clone());

        let count = use_case
            .extract_and_save(
                "fn main() {}",
                "test.rs",
                Language::Rust,
                "repo-1",
                &HashMap::new(),
            )
            .await
            .unwrap();

        assert_eq!(count, 1);
        assert_eq!(repository.saved_count(), 1);
    }

    #[tokio::test]
    async fn test_extract_and_save_empty() {
        let extractor = Arc::new(MockExtractor::new(Vec::new()));
        let repository = Arc::new(MockCallGraphRepository::new());

        let use_case = CallGraphUseCase::new(extractor, repository.clone());

        let count = use_case
            .extract_and_save(
                "fn main() {}",
                "test.rs",
                Language::Rust,
                "repo-1",
                &HashMap::new(),
            )
            .await
            .unwrap();

        assert_eq!(count, 0);
        assert_eq!(repository.saved_count(), 0);
    }
}
