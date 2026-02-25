use std::collections::HashMap;

use async_trait::async_trait;

use crate::domain::{CodeChunk, DomainError, Language, SymbolReference};

/// Parses source code into semantic chunks.
#[async_trait]
pub trait ParserService: Send + Sync {
    /// Parse a file into semantic code chunks (functions, classes, etc.).
    async fn parse_file(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
    ) -> Result<Vec<CodeChunk>, DomainError>;

    /// Extract symbol references (function calls, type references, etc.) from a file.
    async fn extract_references(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
    ) -> Result<Vec<SymbolReference>, DomainError>;

    /// Return the list of symbol names exported by this file (JS/TS only).
    ///
    /// Covers:
    /// - `module.exports = identifier`
    /// - `module.exports.key = …`
    /// - `export default identifier`
    /// - `export function/class/const identifier`
    /// - `export { identifier }`
    ///
    /// Returns an empty `Vec` for unsupported languages or files with no detectable exports.
    fn extract_module_exports(&self, _content: &str, _language: Language) -> Vec<String> {
        Vec::new()
    }

    /// Like `extract_references`, but also resolves relative `require('./path')` calls
    /// against `exports_by_file` to replace local-binding names with the actual exported
    /// symbol names.
    ///
    /// `exports_by_file` maps repo-relative file paths to the list of symbol names that
    /// file exports (populated by a prior pass using `extract_module_exports`).
    ///
    /// The default implementation ignores `exports_by_file` and delegates to
    /// `extract_references`, so adapters that don't implement cross-file resolution
    /// continue to work unchanged.
    async fn extract_references_with_exports(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
        _exports_by_file: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        self.extract_references(content, file_path, language, repository_id)
            .await
    }

    fn supported_languages(&self) -> Vec<Language>;

    fn supports_language(&self, language: Language) -> bool {
        self.supported_languages().contains(&language)
    }
}
