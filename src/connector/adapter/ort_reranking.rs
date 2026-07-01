use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use ndarray::Array4;
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::{Tensor, ValueType},
};
use tokenizers::Tokenizer;
use tracing::{debug, info, warn};

use crate::application::RerankingService;
use crate::domain::{DomainError, SearchResult};

const DEFAULT_MODEL_ID: &str = "onnx-community/Qwen3-Reranker-0.6B-ONNX";
const DEFAULT_MAX_SEQ_LENGTH: usize = 512;
const BATCH_SIZE: usize = 32;

/// Task description embedded in the Qwen reranker prompt's `<Instruct>` slot.
const QWEN_INSTRUCTION: &str =
    "Given a code search query, retrieve relevant code that matches the query";
/// Chat-template prefix that precedes the query/document content for the Qwen3
/// reranker.  The model is a causal LM trained to answer "yes"/"no".
const QWEN_PREFIX: &str = "<|im_start|>system\nJudge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be \"yes\" or \"no\".<|im_end|>\n<|im_start|>user\n";
/// Chat-template suffix that closes the prompt and primes the assistant turn.
const QWEN_SUFFIX: &str = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

struct KvCacheParams {
    num_layers: usize,
    num_heads: usize,
    head_dim: usize,
}

fn detect_kv_cache(session: &Session) -> Option<KvCacheParams> {
    let inputs = session.inputs();
    let num_layers = inputs
        .iter()
        .filter(|i| i.name().starts_with("past_key_values.") && i.name().ends_with(".key"))
        .count();
    if num_layers == 0 {
        return None;
    }
    let (num_heads, head_dim) = inputs
        .iter()
        .find(|i| i.name() == "past_key_values.0.key")
        .and_then(|inp| {
            if let ValueType::Tensor { shape, .. } = inp.dtype() {
                let nh = shape.get(1).copied().filter(|&x| x > 0).unwrap_or(8) as usize;
                let hd = shape.get(3).copied().filter(|&x| x > 0).unwrap_or(128) as usize;
                Some((nh, hd))
            } else {
                None
            }
        })
        .unwrap_or((8, 128));
    Some(KvCacheParams {
        num_layers,
        num_heads,
        head_dim,
    })
}

/// Which scoring scheme the loaded ONNX reranker uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RerankerKind {
    /// BERT-style cross-encoder emitting a single relevance logit per pair
    /// (e.g. BAAI/bge-reranker-base).  Scored with a sigmoid.
    CrossEncoder,
    /// Causal/decoder reranker (e.g. Qwen3-Reranker) that answers "yes"/"no";
    /// scored from the softmax of the yes/no token logits at the last position.
    Causal,
}

pub struct OrtReranking {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Tokenizer>,
    model_name: String,
    max_sequence_length: usize,
    /// True when the loaded ONNX model declares `token_type_ids` as a required
    /// input.  For cross-encoder models the segment IDs from the tokenizer are
    /// used (0 = query tokens, 1 = document tokens).
    needs_token_type_ids: bool,
    /// Scoring scheme inferred from the model architecture at load time.
    kind: RerankerKind,
    /// Vocabulary id of the "yes" token (Causal kind only).
    token_true_id: Option<u32>,
    /// Vocabulary id of the "no" token (Causal kind only).
    token_false_id: Option<u32>,
    /// KV-cache decoder parameters (present for Qwen3-Reranker ONNX exports).
    kv_cache: Option<KvCacheParams>,
}

impl OrtReranking {
    pub fn new(model_id: Option<&str>) -> Result<Self, DomainError> {
        let model_id = model_id.unwrap_or(DEFAULT_MODEL_ID);
        debug!(
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

        // Prefer the self-contained quantized model (no .onnx_data sidecar).
        // Fall back to the full-precision model for custom repos that don't
        // ship a quantized variant.
        let model_path = repo
            .get("onnx/model_quantized.onnx")
            .or_else(|_| repo.get("model.onnx"))
            .or_else(|_| repo.get("onnx/model.onnx"))
            .map_err(|e| DomainError::internal(format!("Failed to download ONNX model: {}", e)))?;

        Self::from_paths(model_path, tokenizer_path, model_id)
    }

    pub fn from_paths(
        model_path: PathBuf,
        tokenizer_path: PathBuf,
        model_name: &str,
    ) -> Result<Self, DomainError> {
        debug!("Loading ONNX model from: {:?}", model_path);

        let session = Session::builder()
            .map_err(|e| DomainError::internal(format!("Failed to create session builder: {}", e)))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| DomainError::internal(format!("Failed to set optimization level: {}", e)))?
            .commit_from_file(&model_path)
            .map_err(|e| DomainError::internal(format!("Failed to load ONNX model: {}", e)))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| DomainError::internal(format!("Failed to load tokenizer: {}", e)))?;

        let needs_token_type_ids = session
            .inputs()
            .iter()
            .any(|i| i.name() == "token_type_ids");

        // A BERT-style cross-encoder declares a `token_type_ids` (segment) input;
        // a causal/decoder reranker (Qwen3-Reranker) does not.  The latter is
        // scored from yes/no token logits, so resolve those vocabulary ids up
        // front.
        let kind = if needs_token_type_ids {
            RerankerKind::CrossEncoder
        } else {
            RerankerKind::Causal
        };
        let (token_true_id, token_false_id) = if kind == RerankerKind::Causal {
            let yes = tokenizer.token_to_id("yes");
            let no = tokenizer.token_to_id("no");
            if yes.is_none() || no.is_none() {
                warn!(
                    "OrtReranking: causal reranker '{}' is missing a 'yes'/'no' token \
                     in its vocabulary; reranking will preserve retrieval order",
                    model_name
                );
            }
            (yes, no)
        } else {
            (None, None)
        };
        let kv_cache = detect_kv_cache(&session);
        if kv_cache.is_some() {
            debug!("Reranker: KV-cache decoder detected ({} layers)", kv_cache.as_ref().unwrap().num_layers);
        }
        debug!("Reranker kind: {:?}", kind);

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tokenizer: Arc::new(tokenizer),
            model_name: model_name.to_string(),
            max_sequence_length: DEFAULT_MAX_SEQ_LENGTH,
            needs_token_type_ids,
            kind,
            token_true_id,
            token_false_id,
            kv_cache,
        })
    }

    fn rerank_batch(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>, DomainError> {
        if documents.is_empty() {
            return Ok(vec![]);
        }
        match self.kind {
            RerankerKind::CrossEncoder => self.rerank_batch_cross_encoder(query, documents),
            RerankerKind::Causal => self.rerank_batch_causal(query, documents),
        }
    }

    /// BERT-style cross-encoder scoring: tokenize query/document pairs and apply
    /// a sigmoid to the single relevance logit.
    fn rerank_batch_cross_encoder(
        &self,
        query: &str,
        documents: &[&str],
    ) -> Result<Vec<f32>, DomainError> {
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
        let mut token_type_ids: Vec<i64> = Vec::with_capacity(batch_size * max_len);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();
            let type_ids = encoding.get_type_ids();

            let len = ids.len().min(max_len);

            input_ids.extend(ids[..len].iter().map(|&x| x as i64));
            attention_mask.extend(mask[..len].iter().map(|&x| x as i64));
            token_type_ids.extend(type_ids[..len].iter().map(|&x| x as i64));

            let padding = max_len - len;
            input_ids.extend(std::iter::repeat_n(0i64, padding));
            attention_mask.extend(std::iter::repeat_n(0i64, padding));
            token_type_ids.extend(std::iter::repeat_n(0i64, padding));
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

        let outputs = if self.needs_token_type_ids {
            let token_type_ids_tensor =
                Tensor::from_array((shape, token_type_ids)).map_err(|e| {
                    DomainError::internal(format!("Failed to create token_type_ids tensor: {}", e))
                })?;
            session.run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            ])
        } else {
            session.run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            ])
        }
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

    /// Causal (Qwen3-Reranker) scoring: wrap each query/document pair in the
    /// reranker chat template, run the decoder, and take the softmax of the
    /// "yes"/"no" token logits at the last real token as the relevance score.
    ///
    /// When the vocabulary lacks a yes/no token, returns neutral scores so the
    /// caller's stable sort preserves the original retrieval order rather than
    /// failing the whole search.
    fn rerank_batch_causal(
        &self,
        query: &str,
        documents: &[&str],
    ) -> Result<Vec<f32>, DomainError> {
        let batch_size = documents.len();

        let (Some(yes_id), Some(no_id)) = (self.token_true_id, self.token_false_id) else {
            return Ok(vec![0.5; batch_size]);
        };
        let (yes_id, no_id) = (yes_id as usize, no_id as usize);

        // Build the prompt body (everything except the trailing assistant
        // suffix) separately so the suffix — which holds the position where the
        // model emits its yes/no answer — always survives truncation of a long
        // document rather than being dropped off the end.
        let bodies: Vec<String> = documents
            .iter()
            .map(|doc| {
                format!(
                    "{QWEN_PREFIX}<Instruct>: {QWEN_INSTRUCTION}\n<Query>: {query}\n<Document>: {doc}"
                )
            })
            .collect();

        // The chat-template markers (`<|im_start|>`, `<think>`, …) are added
        // tokens the tokenizer maps directly, so no extra special tokens are
        // injected here.
        let suffix_ids: Vec<i64> = self
            .tokenizer
            .encode(QWEN_SUFFIX, false)
            .map_err(|e| DomainError::internal(format!("Tokenization failed: {}", e)))?
            .get_ids()
            .iter()
            .map(|&x| x as i64)
            .collect();
        let suffix_len = suffix_ids.len();
        // Tokens left for the body once room for the suffix is reserved.
        let body_budget = self.max_sequence_length.saturating_sub(suffix_len);

        let encodings = self
            .tokenizer
            .encode_batch(bodies.iter().map(|s| s.as_str()).collect(), false)
            .map_err(|e| DomainError::internal(format!("Tokenization failed: {}", e)))?;

        let max_body = encodings
            .iter()
            .map(|e| e.get_ids().len().min(body_budget))
            .max()
            .unwrap_or(0);
        let max_len = max_body + suffix_len;

        let mut input_ids: Vec<i64> = Vec::with_capacity(batch_size * max_len);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(batch_size * max_len);
        // Index of the last real (non-padded) token per sequence: the final
        // suffix token, where the causal LM emits its yes/no answer logits.
        let mut last_idx: Vec<usize> = Vec::with_capacity(batch_size);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let body_len = ids.len().min(body_budget);

            // Truncated body followed by the full (always-present) suffix.
            input_ids.extend(ids[..body_len].iter().map(|&x| x as i64));
            input_ids.extend(suffix_ids.iter().copied());

            let real_len = body_len + suffix_len;
            attention_mask.extend(std::iter::repeat_n(1i64, real_len));

            let padding = max_len - real_len;
            input_ids.extend(std::iter::repeat_n(0i64, padding));
            attention_mask.extend(std::iter::repeat_n(0i64, padding));

            last_idx.push(real_len.saturating_sub(1));
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

        let outputs = if let Some(kv) = &self.kv_cache {
            let pos_ids: Vec<i64> = (0..max_len as i64)
                .cycle()
                .take(batch_size * max_len)
                .collect();
            let pos_ids_tensor = Tensor::from_array(([batch_size, max_len], pos_ids)).map_err(
                |e| DomainError::internal(format!("Failed to create position_ids tensor: {}", e)),
            )?;
            let mut inputs_map: HashMap<String, ort::value::Value> = HashMap::new();
            inputs_map.insert("input_ids".to_string(), input_ids_tensor.into());
            inputs_map.insert("attention_mask".to_string(), attention_mask_tensor.into());
            inputs_map.insert("position_ids".to_string(), pos_ids_tensor.into());
            for layer in 0..kv.num_layers {
                let empty_k: Array4<f32> =
                    Array4::zeros([batch_size, kv.num_heads, 0, kv.head_dim]);
                let empty_v: Array4<f32> =
                    Array4::zeros([batch_size, kv.num_heads, 0, kv.head_dim]);
                inputs_map.insert(
                    format!("past_key_values.{layer}.key"),
                    Tensor::from_array(empty_k)
                        .map_err(|e| DomainError::internal(format!("KV tensor error: {}", e)))?
                        .into(),
                );
                inputs_map.insert(
                    format!("past_key_values.{layer}.value"),
                    Tensor::from_array(empty_v)
                        .map_err(|e| DomainError::internal(format!("KV tensor error: {}", e)))?
                        .into(),
                );
            }
            session.run(inputs_map)
        } else {
            session.run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            ])
        }
        .map_err(|e| DomainError::internal(format!("Inference failed: {}", e)))?;

        let output_value = outputs
            .iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| DomainError::internal("No output tensor found"))?;

        let (out_shape, data) = output_value.try_extract_tensor::<f32>().map_err(|e| {
            DomainError::internal(format!("Failed to extract output tensor: {}", e))
        })?;

        let out_shape: Vec<usize> = out_shape.iter().map(|&x| x as usize).collect();
        debug!("Causal reranker output shape: {:?}", out_shape);

        // Expect causal LM logits: [batch, seq, vocab].
        if out_shape.len() != 3 {
            return Err(DomainError::internal(format!(
                "Unexpected causal reranker output shape: {:?} (expected [batch, seq, vocab])",
                out_shape
            )));
        }
        let seq_len = out_shape[1];
        let vocab = out_shape[2];
        if yes_id >= vocab || no_id >= vocab {
            return Err(DomainError::internal(format!(
                "yes/no token id out of range for vocab {vocab}"
            )));
        }

        let scores = (0..batch_size)
            .map(|i| {
                let pos = last_idx[i].min(seq_len.saturating_sub(1));
                let row = i * seq_len * vocab + pos * vocab;
                let yes_logit = data[row + yes_id];
                let no_logit = data[row + no_id];
                // Two-way softmax over the yes/no logits.
                let m = yes_logit.max(no_logit);
                let yes_e = (yes_logit - m).exp();
                let no_e = (no_logit - m).exp();
                yes_e / (yes_e + no_e)
            })
            .collect();

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

        let start_time = Instant::now();

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

        let duration = start_time.elapsed();
        info!(
            "Reranking complete: {} results in {:.2}s",
            reranked.len(),
            duration.as_secs_f64()
        );

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
