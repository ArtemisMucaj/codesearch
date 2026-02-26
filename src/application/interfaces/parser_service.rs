use async_trait::async_trait;

use crate::domain::{CodeChunk, DomainError, Language};

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

    fn supported_languages(&self) -> Vec<Language>;

    fn supports_language(&self, language: Language) -> bool {
        self.supported_languages().contains(&language)
    }
}
