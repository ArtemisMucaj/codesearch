use serde::{Deserialize, Serialize};

use super::CodeChunk;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub chunk: CodeChunk,
    pub score: f32,
    pub highlights: Option<Vec<String>>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    pub limit: usize,
    pub min_score: Option<f32>,
    pub languages: Option<Vec<String>>,
    pub repository_ids: Option<Vec<String>>,
    pub node_types: Option<Vec<String>>,
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
        }
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
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
}
