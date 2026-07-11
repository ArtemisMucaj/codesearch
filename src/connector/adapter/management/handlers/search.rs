//! Search endpoint — `POST /api/search`.
//!
//! Body maps onto the hybrid search use case (the same one the CLI `search`
//! command drives). Returns a JSON array of structured results.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::domain::{SearchQuery, SearchResult};

use super::super::error::ApiResult;
use super::super::server::AppState;

/// Default number of results when the request omits `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 10;

/// Request body for `POST /api/search`.
#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    /// Natural-language query describing what the code does.
    pub query: String,
    /// Maximum number of results (defaults to [`DEFAULT_SEARCH_LIMIT`]).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Optional minimum relevance score filter.
    #[serde(default)]
    pub min_score: Option<f32>,
    /// Optional language filter (e.g. `["rust", "python"]`).
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    /// Optional repository filter (names or UUIDs).
    #[serde(default)]
    pub repositories: Option<Vec<String>>,
    /// Whether to include the keyword (BM25) leg. Defaults to `true`.
    #[serde(default = "default_text_search")]
    pub text_search: bool,
}

fn default_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

fn default_text_search() -> bool {
    true
}

/// A single structured search result on the wire.
#[derive(Debug, Serialize)]
struct SearchHit {
    file_path: String,
    start_line: u32,
    end_line: u32,
    score: f32,
    language: String,
    node_type: String,
    symbol_name: Option<String>,
    content: String,
}

impl SearchHit {
    fn from_result(result: &SearchResult) -> Self {
        let chunk = result.chunk();
        Self {
            file_path: chunk.file_path().to_string(),
            start_line: chunk.start_line(),
            end_line: chunk.end_line(),
            score: result.score(),
            language: chunk.language().to_string(),
            node_type: chunk.node_type().as_str().to_string(),
            symbol_name: chunk.symbol_name().map(str::to_string),
            content: chunk.content().to_string(),
        }
    }
}

/// `POST /api/search` — hybrid semantic + keyword search.
pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut query = SearchQuery::new(&req.query)
        .with_limit(req.limit)
        .with_text_search(req.text_search);

    if let Some(score) = req.min_score {
        query = query.with_min_score(score);
    }
    if let Some(languages) = req.languages {
        query = query.with_languages(languages);
    }
    if let Some(repositories) = req.repositories {
        query = query.with_repositories(repositories);
    }

    let results = state.container.search_use_case().execute(query).await?;
    let hits: Vec<SearchHit> = results.iter().map(SearchHit::from_result).collect();

    Ok(Json(serde_json::json!({
        "count": hits.len(),
        "results": hits,
    })))
}
