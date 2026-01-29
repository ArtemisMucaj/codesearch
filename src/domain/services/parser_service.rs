use async_trait::async_trait;

use crate::domain::{CodeChunk, DomainError, Language};

#[async_trait]
pub trait ParserService: Send + Sync {
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
