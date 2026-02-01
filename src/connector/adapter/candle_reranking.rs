use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::qwen3::{Config as Qwen3Config, ModelForCausalLM};
use tokenizers::Tokenizer;
use tracing::{debug, info};

use crate::application::RerankingService;
use crate::domain::{DomainError, SearchResult};

const DEFAULT_MODEL_ID: &str = "Qwen/Qwen3-Reranker-0.6B";
const DEFAULT_MAX_SEQ_LENGTH: usize = 8192;
const BATCH_SIZE: usize = 8; // Smaller batch size for larger model

/// Qwen3-based reranker that uses generative scoring (yes/no prediction)
pub struct CandleReranking {
    model: Mutex<ModelForCausalLM>,
    tokenizer: Tokenizer,
    device: Device,
    #[allow(dead_code)]
    dtype: DType,
    model_name: String,
    max_sequence_length: usize,
    // Token IDs for "yes" and "no"
    token_yes_id: u32,
    token_no_id: u32,
    // Prefix and suffix tokens for the prompt format
    prefix_tokens: Vec<u32>,
    suffix_tokens: Vec<u32>,
}

impl CandleReranking {
    /// Create a new CandleReranking service using the specified model or the default.
    /// Set `use_gpu` to true to attempt GPU acceleration (CUDA or Metal).
    pub fn new(model_id: Option<&str>, use_gpu: bool) -> Result<Self, DomainError> {
        let model_id = model_id.unwrap_or(DEFAULT_MODEL_ID);
        info!(
            "Initializing Candle reranking service with model: {} (GPU: {})",
            model_id, use_gpu
        );

        let device = Self::select_device(use_gpu)?;
        info!("Using device: {:?}", device);

        let api = hf_hub::api::sync::ApiBuilder::new()
            .with_progress(true)
            .build()
            .map_err(|e| DomainError::internal(format!("Failed to create HF API: {}", e)))?;

        let repo = api.model(model_id.to_string());

        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| DomainError::internal(format!("Failed to download tokenizer: {}", e)))?;

        let config_path = repo
            .get("config.json")
            .map_err(|e| DomainError::internal(format!("Failed to download config: {}", e)))?;

        let weights_path = repo
            .get("model.safetensors")
            .or_else(|_| repo.get("pytorch_model.bin"))
            .map_err(|e| {
                DomainError::internal(format!("Failed to download model weights: {}", e))
            })?;

        Self::from_paths(weights_path, config_path, tokenizer_path, model_id, device)
    }

    /// Create from local file paths
    pub fn from_paths(
        weights_path: PathBuf,
        config_path: PathBuf,
        tokenizer_path: PathBuf,
        model_name: &str,
        device: Device,
    ) -> Result<Self, DomainError> {
        info!("Loading Candle reranking model from: {:?}", weights_path);

        // Determine dtype based on device
        let dtype = if matches!(device, Device::Cuda(_)) {
            DType::BF16
        } else {
            DType::F32
        };

        // Load config
        let config_content = std::fs::read_to_string(&config_path)
            .map_err(|e| DomainError::internal(format!("Failed to read config: {}", e)))?;
        let qwen3_config: Qwen3Config = serde_json::from_str(&config_content)
            .map_err(|e| DomainError::internal(format!("Failed to parse config: {}", e)))?;

        // Load weights
        let vb = if weights_path
            .extension()
            .map_or(false, |e| e == "safetensors")
        {
            unsafe {
                VarBuilder::from_mmaped_safetensors(&[weights_path], dtype, &device).map_err(
                    |e| DomainError::internal(format!("Failed to load safetensors: {}", e)),
                )?
            }
        } else {
            VarBuilder::from_pth(&weights_path, dtype, &device).map_err(|e| {
                DomainError::internal(format!("Failed to load PyTorch weights: {}", e))
            })?
        };

        // Build model
        let model = ModelForCausalLM::new(&qwen3_config, vb).map_err(|e| {
            DomainError::internal(format!("Failed to build Qwen3 model: {}", e))
        })?;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| DomainError::internal(format!("Failed to load tokenizer: {}", e)))?;

        // Get token IDs for "yes" and "no"
        let token_yes_id = tokenizer
            .token_to_id("yes")
            .ok_or_else(|| DomainError::internal("Token 'yes' not found in vocabulary"))?;
        let token_no_id = tokenizer
            .token_to_id("no")
            .ok_or_else(|| DomainError::internal("Token 'no' not found in vocabulary"))?;

        info!(
            "Reranker token IDs: yes={}, no={}",
            token_yes_id, token_no_id
        );

        // Prepare prefix and suffix tokens for the prompt format
        let prefix = "<|im_start|>system\nJudge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be \"yes\" or \"no\".<|im_end|>\n<|im_start|>user\n";
        let suffix = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

        let prefix_tokens = tokenizer
            .encode(prefix, false)
            .map_err(|e| DomainError::internal(format!("Failed to encode prefix: {}", e)))?
            .get_ids()
            .to_vec();

        let suffix_tokens = tokenizer
            .encode(suffix, false)
            .map_err(|e| DomainError::internal(format!("Failed to encode suffix: {}", e)))?
            .get_ids()
            .to_vec();

        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            device,
            dtype,
            model_name: model_name.to_string(),
            max_sequence_length: DEFAULT_MAX_SEQ_LENGTH,
            token_yes_id,
            token_no_id,
            prefix_tokens,
            suffix_tokens,
        })
    }

    fn select_device(use_gpu: bool) -> Result<Device, DomainError> {
        if use_gpu {
            #[cfg(feature = "cuda")]
            {
                if let Ok(device) = Device::new_cuda(0) {
                    info!("CUDA device available, using GPU");
                    return Ok(device);
                }
            }

            #[cfg(feature = "metal")]
            {
                if let Ok(device) = Device::new_metal(0) {
                    info!("Metal device available, using GPU");
                    return Ok(device);
                }
            }

            info!("No GPU available");
        }

        Ok(Device::Cpu)
    }

    /// Format query-document pair for the reranker
    fn format_input(&self, instruction: &str, query: &str, document: &str) -> String {
        format!(
            "<Instruct>: {}\n<Query>: {}\n<Document>: {}",
            instruction, query, document
        )
    }

    /// Tokenize a single query-document pair with prefix and suffix
    fn tokenize_pair(&self, query: &str, document: &str) -> Result<Vec<u32>, DomainError> {
        let instruction = "Given a code search query, determine if the code snippet is relevant to the query";
        let formatted = self.format_input(instruction, query, document);

        let content_tokens = self
            .tokenizer
            .encode(formatted.as_str(), false)
            .map_err(|e| DomainError::internal(format!("Tokenization failed: {}", e)))?
            .get_ids()
            .to_vec();

        // Calculate max content length
        let max_content_len =
            self.max_sequence_length - self.prefix_tokens.len() - self.suffix_tokens.len();

        // Truncate content if necessary
        let content_tokens = if content_tokens.len() > max_content_len {
            content_tokens[..max_content_len].to_vec()
        } else {
            content_tokens
        };

        // Combine: prefix + content + suffix
        let mut full_tokens = Vec::with_capacity(
            self.prefix_tokens.len() + content_tokens.len() + self.suffix_tokens.len(),
        );
        full_tokens.extend_from_slice(&self.prefix_tokens);
        full_tokens.extend_from_slice(&content_tokens);
        full_tokens.extend_from_slice(&self.suffix_tokens);

        Ok(full_tokens)
    }

    /// Compute relevance score for a single query-document pair
    fn compute_score(&self, tokens: &[u32]) -> Result<f32, DomainError> {
        let seq_len = tokens.len();

        // Create input tensor
        let input_ids = Tensor::from_vec(tokens.to_vec(), (1, seq_len), &self.device)
            .map_err(|e| DomainError::internal(format!("Failed to create input tensor: {}", e)))?;

        // Run forward pass
        let mut model = self
            .model
            .lock()
            .map_err(|e| DomainError::internal(format!("Failed to lock model: {}", e)))?;

        // Clear KV cache before processing
        model.clear_kv_cache();

        // Get logits for the last token
        let logits = model
            .forward(&input_ids, 0)
            .map_err(|e| DomainError::internal(format!("Inference failed: {}", e)))?;

        // logits shape is (batch, 1, vocab_size), squeeze to (vocab_size,)
        let logits = logits
            .squeeze(0)
            .map_err(|e| DomainError::internal(format!("Failed to squeeze batch dim: {}", e)))?
            .squeeze(0)
            .map_err(|e| DomainError::internal(format!("Failed to squeeze seq dim: {}", e)))?
            .to_dtype(DType::F32)
            .map_err(|e| DomainError::internal(format!("Failed to convert dtype: {}", e)))?;

        // Extract logits for "yes" and "no" tokens
        let yes_logit: f32 = logits
            .i(self.token_yes_id as usize)
            .map_err(|e| DomainError::internal(format!("Failed to get yes logit: {}", e)))?
            .to_scalar()
            .map_err(|e| DomainError::internal(format!("Failed to convert yes logit: {}", e)))?;

        let no_logit: f32 = logits
            .i(self.token_no_id as usize)
            .map_err(|e| DomainError::internal(format!("Failed to get no logit: {}", e)))?
            .to_scalar()
            .map_err(|e| DomainError::internal(format!("Failed to convert no logit: {}", e)))?;

        // Compute softmax probability for "yes"
        let max_logit = yes_logit.max(no_logit);
        let yes_exp = (yes_logit - max_logit).exp();
        let no_exp = (no_logit - max_logit).exp();
        let score = yes_exp / (yes_exp + no_exp);

        Ok(score)
    }

    fn rerank_batch(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>, DomainError> {
        if documents.is_empty() {
            return Ok(vec![]);
        }

        let mut scores = Vec::with_capacity(documents.len());

        for doc in documents {
            let tokens = self.tokenize_pair(query, doc)?;
            let score = self.compute_score(&tokens)?;
            scores.push(score);
        }

        Ok(scores)
    }
}

#[async_trait]
impl RerankingService for CandleReranking {
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

// Expose the device for testing
impl CandleReranking {
    pub fn device(&self) -> &Device {
        &self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CodeChunk, Language, NodeType};

    #[tokio::test]
    #[ignore = "Requires model download"]
    async fn test_candle_reranking_service() {
        let service = CandleReranking::new(None, false).expect("Failed to create service");

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
        assert!(reranked[0].chunk().content().contains("add"));
        assert!(reranked[0].score() >= 0.0 && reranked[0].score() <= 1.0);
    }
}
