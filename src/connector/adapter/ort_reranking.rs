use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use tokenizers::Tokenizer;
use tracing::{debug, info, warn};

#[cfg(feature = "cuda")]
use ort::execution_providers::CUDAExecutionProvider;

#[cfg(feature = "coreml")]
use ort::execution_providers::CoreMLExecutionProvider;

use crate::application::RerankingService;
use crate::domain::{DomainError, SearchResult};

const DEFAULT_MODEL_ID: &str = "mixedbread-ai/mxbai-rerank-xsmall-v1";
const DEFAULT_MAX_SEQ_LENGTH: usize = 512;
const BATCH_SIZE: usize = 32;

pub struct OrtReranking {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Tokenizer>,
    model_name: String,
    max_sequence_length: usize,
}

impl OrtReranking {
    pub fn new(model_id: Option<&str>) -> Result<Self, DomainError> {
        let model_id = model_id.unwrap_or(DEFAULT_MODEL_ID);
        info!(
            "Initializing ORT reranking service with model: {}",
            model_id
        );

        let api = hf_hub::api::sync::ApiBuilder::new()
            .with_progress(true)
            .build()
            .map_err(|e| DomainError::internal(format!("Failed to create HF API: {}", e)))?;

        let repo = api.model(model_id.to_string());

        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| DomainError::internal(format!("Failed to download tokenizer: {}", e)))?;

        let model_path = repo
            .get("model.onnx")
            .or_else(|_| repo.get("onnx/model.onnx"))
            .map_err(|e| DomainError::internal(format!("Failed to download ONNX model: {}", e)))?;

        Self::from_paths(model_path, tokenizer_path, model_id)
    }

    pub fn from_paths(
        model_path: PathBuf,
        tokenizer_path: PathBuf,
        model_name: &str,
    ) -> Result<Self, DomainError> {
        info!("Loading ONNX model from: {:?}", model_path);

        let session = Self::create_session(&model_path)?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| DomainError::internal(format!("Failed to load tokenizer: {}", e)))?;

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tokenizer: Arc::new(tokenizer),
            model_name: model_name.to_string(),
            max_sequence_length: DEFAULT_MAX_SEQ_LENGTH,
        })
    }

    fn create_session(model_path: &PathBuf) -> Result<Session, DomainError> {
        let builder = Session::builder().map_err(|e| {
            DomainError::internal(format!("Failed to create session builder: {}", e))
        })?;

        #[cfg(feature = "cuda")]
        let builder = {
            let cuda_available = CUDAExecutionProvider::is_available();
            if cuda_available {
                info!("CUDA execution provider available, enabling GPU acceleration");
            } else {
                warn!("CUDA execution provider not available (missing CUDA/cuDNN?), falling back to CPU");
            }
            builder
                .with_execution_providers([CUDAExecutionProvider::default().build()])
                .map_err(|e| {
                    DomainError::internal(format!("Failed to set CUDA execution provider: {}", e))
                })?
        };

        #[cfg(feature = "coreml")]
        let builder = {
            let coreml_available = CoreMLExecutionProvider::is_available();
            if coreml_available {
                info!("CoreML execution provider available, enabling GPU/ANE acceleration");
            } else {
                warn!("CoreML execution provider not available, falling back to CPU");
            }
            builder
                .with_execution_providers([CoreMLExecutionProvider::default()
                    .with_subgraphs()
                    .build()])
                .map_err(|e| {
                    DomainError::internal(format!("Failed to set CoreML execution provider: {}", e))
                })?
        };

        #[cfg(not(any(feature = "cuda", feature = "coreml")))]
        info!("No GPU execution provider configured, using CPU");

        builder
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| DomainError::internal(format!("Failed to set optimization level: {}", e)))?
            .commit_from_file(model_path)
            .map_err(|e| DomainError::internal(format!("Failed to load ONNX model: {}", e)))
    }

    fn rerank_batch(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>, DomainError> {
        if documents.is_empty() {
            return Ok(vec![]);
        }

        let batch_size = documents.len();

        // Tokenize query-document pairs
        let text_pairs: Vec<(String, String)> = documents
            .iter()
            .map(|doc| (query.to_string(), doc.to_string()))
            .collect();

        let encodings = self
            .tokenizer
            .encode_batch(
                text_pairs
                    .iter()
                    .map(|(q, d)| (q.as_str(), d.as_str()))
                    .collect(),
                true,
            )
            .map_err(|e| DomainError::internal(format!("Tokenization failed: {}", e)))?;

        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .min(self.max_sequence_length);

        let mut input_ids: Vec<i64> = Vec::with_capacity(batch_size * max_len);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(batch_size * max_len);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();

            let len = ids.len().min(max_len);

            input_ids.extend(ids[..len].iter().map(|&x| x as i64));
            attention_mask.extend(mask[..len].iter().map(|&x| x as i64));

            let padding = max_len - len;
            input_ids.extend(std::iter::repeat_n(0i64, padding));
            attention_mask.extend(std::iter::repeat_n(0i64, padding));
        }

        let shape = [batch_size, max_len];
        let input_ids_tensor = Tensor::from_array((shape, input_ids)).map_err(|e| {
            DomainError::internal(format!("Failed to create input_ids tensor: {}", e))
        })?;
        let attention_mask_tensor = Tensor::from_array((shape, attention_mask)).map_err(|e| {
            DomainError::internal(format!("Failed to create attention_mask tensor: {}", e))
        })?;

        let mut session = self
            .session
            .lock()
            .map_err(|e| DomainError::internal(format!("Failed to lock session: {}", e)))?;

        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            ])
            .map_err(|e| DomainError::internal(format!("Inference failed: {}", e)))?;

        let output_value = outputs
            .iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| DomainError::internal("No output tensor found"))?;

        let (shape, data) = output_value.try_extract_tensor::<f32>().map_err(|e| {
            DomainError::internal(format!("Failed to extract output tensor: {}", e))
        })?;

        let shape: Vec<usize> = shape.iter().map(|&x| x as usize).collect();
        debug!("Output tensor shape: {:?}", shape);

        // Extract logits and apply sigmoid normalization
        let scores = if shape.len() == 2 && shape[1] == 1 {
            // Shape: [batch_size, 1] - direct logits
            data.iter()
                .step_by(1)
                .take(batch_size)
                .map(|&logit| sigmoid(logit))
                .collect()
        } else if shape.len() == 1 {
            // Shape: [batch_size] - already squeezed
            data.iter()
                .take(batch_size)
                .map(|&logit| sigmoid(logit))
                .collect()
        } else {
            return Err(DomainError::internal(format!(
                "Unexpected output tensor shape: {:?}",
                shape
            )));
        };

        Ok(scores)
    }
}

#[async_trait]
impl RerankingService for OrtReranking {
    async fn rerank(
        &self,
        query: &str,
        results: Vec<SearchResult>,
        top_k: Option<usize>,
    ) -> Result<Vec<SearchResult>, DomainError> {
        if results.is_empty() {
            return Ok(vec![]);
        }

        info!("Reranking {} results for query: {}", results.len(), query);

        let documents: Vec<String> = results.iter().map(format_document_for_reranking).collect();

        let doc_refs: Vec<&str> = documents.iter().map(|s| s.as_str()).collect();

        let mut all_scores = Vec::with_capacity(results.len());

        for batch_docs in doc_refs.chunks(BATCH_SIZE) {
            let scores = self.rerank_batch(query, batch_docs)?;
            all_scores.extend(scores);
        }

        let mut reranked: Vec<SearchResult> = results
            .into_iter()
            .zip(all_scores)
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

        debug!("Reranking complete, returning {} results", reranked.len());

        Ok(reranked)
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn format_document_for_reranking(result: &SearchResult) -> String {
    let chunk = result.chunk();
    let mut doc = String::new();

    if let Some(symbol) = chunk.symbol_name() {
        doc.push_str(&format!("{} ", symbol));
    }

    doc.push_str(&format!("[{}] ", chunk.node_type()));
    doc.push_str(chunk.content());

    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CodeChunk, Language, NodeType};

    #[tokio::test]
    #[ignore = "Requires model download"]
    async fn test_ort_reranking_service() {
        let service = OrtReranking::new(None).expect("Failed to create service");

        let chunks = vec![
            CodeChunk::new(
                "test.rs".to_string(),
                "fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
                1,
                1,
                Language::Rust,
                NodeType::Function,
                "repo1".to_string(),
            )
            .with_symbol_name("add"),
            CodeChunk::new(
                "test2.rs".to_string(),
                "fn multiply(a: i32, b: i32) -> i32 { a * b }".to_string(),
                1,
                1,
                Language::Rust,
                NodeType::Function,
                "repo1".to_string(),
            )
            .with_symbol_name("multiply"),
        ];

        let results: Vec<SearchResult> = chunks
            .into_iter()
            .map(|c| SearchResult::new(c, 0.5))
            .collect();

        let reranked = service
            .rerank("function that adds two numbers", results, None)
            .await
            .unwrap();

        assert_eq!(reranked.len(), 2);
        // First result should be "add" function (more relevant to the query)
        assert!(reranked[0].chunk().content().contains("add"));
        // Scores should be normalized between 0 and 1
        assert!(reranked[0].score() >= 0.0 && reranked[0].score() <= 1.0);
    }
}
