use serde::{Deserialize, Serialize};

use super::CodeChunk;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    chunk: CodeChunk,
    score: f32,
    highlights: Option<Vec<String>>,
}

impl SearchResult {
    pub fn new(chunk: CodeChunk, score: f32) -> Self {
        Self {
            chunk,
            score,
            highlights: None,
        }
    }

    pub fn with_highlights(mut self, highlights: Vec<String>) -> Self {
        self.highlights = Some(highlights);
        self
    }

    pub fn chunk(&self) -> &CodeChunk {
        &self.chunk
    }

    pub fn score(&self) -> f32 {
        self.score
    }

    pub fn highlights(&self) -> Option<&[String]> {
        self.highlights.as_deref()
    }

    pub fn is_relevant(&self, threshold: f32) -> bool {
        self.score >= threshold
    }

    pub fn has_highlights(&self) -> bool {
        self.highlights.as_ref().is_some_and(|h| !h.is_empty())
    }

    pub fn display_line(&self) -> String {
        format!("{} (score: {:.3})", self.chunk.location(), self.score)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    query: String,
    limit: usize,
    min_score: Option<f32>,
    languages: Option<Vec<String>>,
    repository_ids: Option<Vec<String>>,
    node_types: Option<Vec<String>>,
    hybrid: bool,
}

impl SearchQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: 10,
            min_score: None,
            languages: None,
            repository_ids: None,
            node_types: None,
            hybrid: false,
        }
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        // Ensure at least 1 result is requested
        self.limit = limit.max(1);
        self
    }

    pub fn with_min_score(mut self, score: f32) -> Self {
        self.min_score = Some(score);
        self
    }

    pub fn with_languages(mut self, languages: Vec<String>) -> Self {
        self.languages = Some(languages);
        self
    }

    pub fn with_repositories(mut self, ids: Vec<String>) -> Self {
        self.repository_ids = Some(ids);
        self
    }

    pub fn with_node_types(mut self, types: Vec<String>) -> Self {
        self.node_types = Some(types);
        self
    }

    pub fn with_hybrid(mut self, hybrid: bool) -> Self {
        self.hybrid = hybrid;
        self
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn min_score(&self) -> Option<f32> {
        self.min_score
    }

    pub fn languages(&self) -> Option<&[String]> {
        self.languages.as_deref()
    }

    pub fn repository_ids(&self) -> Option<&[String]> {
        self.repository_ids.as_deref()
    }

    pub fn node_types(&self) -> Option<&[String]> {
        self.node_types.as_deref()
    }

    pub fn is_hybrid(&self) -> bool {
        self.hybrid
    }

    pub fn has_filters(&self) -> bool {
        self.languages.is_some() || self.repository_ids.is_some() || self.node_types.is_some()
    }

    pub fn filters_by_language(&self, language: &str) -> bool {
        self.languages
            .as_ref()
            .is_some_and(|langs| langs.iter().any(|l| l == language))
    }

    pub fn filters_by_repository(&self, repo_id: &str) -> bool {
        self.repository_ids
            .as_ref()
            .is_some_and(|ids| ids.contains(&repo_id.to_string()))
    }

    pub fn summary(&self) -> String {
        let mut parts = vec![format!("query=\"{}\"", self.query)];
        parts.push(format!("limit={}", self.limit));

        if let Some(score) = self.min_score {
            parts.push(format!("min_score={:.2}", score));
        }
        if let Some(ref langs) = self.languages {
            parts.push(format!("languages={:?}", langs));
        }
        if let Some(ref repos) = self.repository_ids {
            parts.push(format!("repos={:?}", repos));
        }
        if let Some(ref types) = self.node_types {
            parts.push(format!("types={:?}", types));
        }

        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Language, NodeType};

    fn sample_chunk() -> CodeChunk {
        CodeChunk::new(
            "test.rs".to_string(),
            "fn test() {}".to_string(),
            1,
            1,
            Language::Rust,
            NodeType::Function,
            "repo".to_string(),
        )
    }

    #[test]
    fn test_search_result_creation() {
        let chunk = sample_chunk();
        let result = SearchResult::new(chunk, 0.95);

        assert_eq!(result.score(), 0.95);
        assert!(result.is_relevant(0.5));
        assert!(!result.is_relevant(0.99));
    }

    #[test]
    fn test_search_query_builder() {
        let query = SearchQuery::new("find functions")
            .with_limit(20)
            .with_min_score(0.7)
            .with_languages(vec!["rust".to_string()]);

        assert_eq!(query.query(), "find functions");
        assert_eq!(query.limit(), 20);
        assert_eq!(query.min_score(), Some(0.7));
        assert!(query.has_filters());
    }

    #[test]
    fn test_query_filters() {
        let query =
            SearchQuery::new("test").with_languages(vec!["rust".to_string(), "python".to_string()]);

        assert!(query.filters_by_language("rust"));
        assert!(query.filters_by_language("python"));
        assert!(!query.filters_by_language("go"));
    }
}
