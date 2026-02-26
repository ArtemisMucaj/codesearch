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
    ///
    /// `exports_by_file` maps repo-relative file paths to the exported symbol names of
    /// that file.  When non-empty, the parser resolves relative `require('./path')`
    /// calls against the map to replace local-binding names with the actual exported
    /// symbol names.  Pass an empty map for languages that don't need cross-file
    /// resolution.
    async fn extract_references(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
        exports_by_file: &HashMap<String, Vec<String>>,
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
    /// The default implementation always returns an empty `Vec`.
    async fn extract_module_exports(&self, _content: &str, _language: Language) -> Vec<String> {
        Vec::new()
    }

    fn supported_languages(&self) -> Vec<Language>;

    fn supports_language(&self, language: Language) -> bool {
        self.supported_languages().contains(&language)
    }
}
