use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::application::RerankingService;
use crate::domain::{DomainError, SearchResult};

const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const CHAT_PATH: &str = "/v1/chat/completions";

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

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

/// LLM-based reranker using the OpenAI-compatible `/v1/chat/completions`
/// endpoint (e.g. LM Studio running locally).
///
/// For each rerank call the full candidate list is sent in a single prompt; the
/// model returns a JSON array of relevance scores in input order. On any error
/// (unreachable server, parse failure, wrong array length) the adapter falls back
/// to the original retrieval scores so search always returns results.
///
/// **Configuration** (via environment variables):
///
/// | Variable          | Default                    |
/// |-------------------|----------------------------|
/// | `OPENAI_BASE_URL` | `http://localhost:1234`    |
/// | `OPENAI_MODEL`    | `openai-reranker`          |
/// | `OPENAI_API_KEY`  | `""` (not required locally)|
pub struct OpenAiReranking {
    client: reqwest::Client,
    url: String,
    model_name: String,
}

impl OpenAiReranking {
    pub fn new() -> Self {
        let base = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let url = format!("{}{}", base.trim_end_matches('/'), CHAT_PATH);
        let model_name = std::env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| "openai-reranker".to_string());

        debug!("OpenAiReranking: endpoint={}, model={}", url, model_name);

        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            if !key.is_empty() {
                if let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
                    headers.insert(reqwest::header::AUTHORIZATION, val);
                }
            }
        }

        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .default_headers(headers)
                .build()
                .expect("reqwest::Client build failed"),
            url,
            model_name,
        }
    }

    fn build_prompt(query: &str, documents: &[String]) -> String {
        let mut prompt = format!("Query: \"{query}\"\n\nSnippets:\n");
        for (i, doc) in documents.iter().enumerate() {
            let snippet = if doc.len() > MAX_SNIPPET_CHARS {
                &doc[..MAX_SNIPPET_CHARS]
            } else {
                doc.as_str()
            };
            prompt.push_str(&format!("{}. {}\n", i + 1, snippet));
        }
        prompt
    }

    /// Parse a JSON float array from the raw model response.
    /// Returns `None` when parsing fails or the length doesn't match `expected`.
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

        let body = ChatRequest {
            model: self.model_name.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: SYSTEM_PROMPT.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_prompt,
                },
            ],
            temperature: 0.0,
        };

        let scores = match self.client.post(&self.url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<ChatResponse>().await {
                    Ok(chat) => {
                        let text = chat
                            .choices
                            .into_iter()
                            .next()
                            .map(|c| c.message.content)
                            .unwrap_or_default();
                        debug!("OpenAiReranking raw response: {text}");
                        Self::parse_scores(&text, n).unwrap_or_else(|| {
                            warn!("OpenAiReranking: falling back to original retrieval scores");
                            results.iter().map(|r| r.score()).collect()
                        })
                    }
                    Err(e) => {
                        warn!("OpenAiReranking: failed to parse response: {e}. Falling back.");
                        results.iter().map(|r| r.score()).collect()
                    }
                }
            }
            Ok(resp) => {
                warn!(
                    "OpenAiReranking: server returned {}. Falling back to original scores.",
                    resp.status()
                );
                results.iter().map(|r| r.score()).collect()
            }
            Err(e) => {
                warn!("OpenAiReranking: request error: {e}. Falling back to original scores.");
                results.iter().map(|r| r.score()).collect()
            }
        };

        let mut reranked: Vec<SearchResult> = results
            .into_iter()
            .zip(scores)
            .map(|(result, score)| SearchResult::new(result.chunk().clone(), score))
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
