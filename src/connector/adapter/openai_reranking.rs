use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::application::RerankingService;
use crate::connector::adapter::ChatClient;
use crate::domain::{DomainError, SearchResult};

/// Maximum characters of a code snippet included in the ranking prompt.
const MAX_SNIPPET_CHARS: usize = 300;

const SYSTEM_PROMPT: &str = "\
You are a code search relevance scorer. Given a search query and a numbered list \
of code snippets, output a JSON array of relevance scores — one float per snippet \
in the same order as the input. Scores must be between 0.0 (irrelevant) and 1.0 \
(highly relevant). Output ONLY the JSON array, no prose, no markdown, no code fences.

Example input:
Query: \"function that adds two numbers\"
Snippets:
1. fn add(a: i32, b: i32) -> i32 { a + b }
2. fn connect_to_database() -> Result<Connection>

Example output: [0.97, 0.02]";

/// LLM-based reranker that delegates to a [`ChatClient`] using the
/// OpenAI-compatible `/v1/chat/completions` endpoint (e.g. LM Studio).
///
/// For each rerank call the full candidate list is sent in a single prompt; the
/// model returns a JSON array of relevance scores in input order. On any error
/// (unreachable server, parse failure, wrong array length) the adapter falls back
/// to the original retrieval scores so search always returns results.
///
/// The chat client and model are controlled by the following environment variables:
///
/// | Variable          | Default                    |
/// |-------------------|----------------------------|
/// | `OPENAI_BASE_URL` | `http://localhost:1234`    |
/// | `OPENAI_MODEL`    | `openai-chat`              |
/// | `OPENAI_API_KEY`  | `""` (not required locally)|
pub struct OpenAiReranking {
    client: Arc<dyn ChatClient>,
    model_name: String,
}

impl OpenAiReranking {
    pub fn new(client: Arc<dyn ChatClient>) -> Self {
        let model_name = std::env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| "openai-chat".to_string());
        Self { client, model_name }
    }

    fn build_prompt(query: &str, documents: &[String]) -> String {
        let mut prompt = format!("Query: \"{query}\"\n\nSnippets:\n");
        for (i, doc) in documents.iter().enumerate() {
            let snippet = doc
                .char_indices()
                .nth(MAX_SNIPPET_CHARS)
                .map_or(doc.as_str(), |(i, _)| &doc[..i]);
            prompt.push_str(&format!("{}. {}\n", i + 1, snippet));
        }
        prompt
    }

    fn parse_scores(text: &str, expected: usize) -> Option<Vec<f32>> {
        let start = text.find('[')?;
        let end = text.rfind(']')?;
        if start > end {
            return None;
        }
        let scores: Vec<f32> = serde_json::from_str(&text[start..=end]).ok()?;
        if scores.len() != expected {
            warn!(
                "OpenAiReranking: expected {} scores, got {}",
                expected,
                scores.len()
            );
            return None;
        }
        Some(scores)
    }
}

fn format_document(result: &SearchResult) -> String {
    let chunk = result.chunk();
    let mut doc = String::new();
    if let Some(symbol) = chunk.symbol_name() {
        doc.push_str(&format!("{} ", symbol));
    }
    doc.push_str(&format!("[{}] ", chunk.node_type()));
    doc.push_str(chunk.content());
    doc
}

#[async_trait]
impl RerankingService for OpenAiReranking {
    async fn rerank(
        &self,
        query: &str,
        results: Vec<SearchResult>,
        top_k: Option<usize>,
    ) -> Result<Vec<SearchResult>, DomainError> {
        if results.is_empty() {
            return Ok(vec![]);
        }

        debug!(
            "OpenAiReranking: reranking {} results for query: {}",
            results.len(),
            query
        );

        let documents: Vec<String> = results.iter().map(format_document).collect();
        let user_prompt = Self::build_prompt(query, &documents);
        let n = results.len();

        let scores = match self.client.complete(SYSTEM_PROMPT, &user_prompt).await {
            Ok(text) => {
                debug!("OpenAiReranking raw response: {text}");
                Self::parse_scores(&text, n).unwrap_or_else(|| {
                    warn!("OpenAiReranking: falling back to original retrieval scores");
                    results.iter().map(|r| r.score()).collect()
                })
            }
            Err(e) => {
                warn!("OpenAiReranking: client error: {e}. Falling back to original scores.");
                results.iter().map(|r| r.score()).collect()
            }
        };

        let mut reranked: Vec<SearchResult> = results
            .into_iter()
            .zip(scores)
            .map(|(result, score)| {
                let sr = SearchResult::new(result.chunk().clone(), score);
                match result.highlights() {
                    Some(h) => sr.with_highlights(h.to_vec()),
                    None => sr,
                }
            })
            .collect();

        reranked.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(k) = top_k {
            reranked.truncate(k);
        }

        Ok(reranked)
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }
}
