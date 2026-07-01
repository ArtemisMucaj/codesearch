use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ndarray::Array4;
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::{Tensor, ValueType},
};
use tokenizers::Tokenizer;
use tracing::debug;

use crate::application::EmbeddingService;
use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

const DEFAULT_DIMENSIONS: usize = 1024;
const DEFAULT_MAX_SEQ_LENGTH: usize = 512;
/// Number of chunks processed per ONNX inference call.
///
/// Larger batches amortise per-call overhead and improve CPU utilisation
/// via wider SIMD/matrix operations.  Qwen3-Embedding-0.6B (600M params,
/// 1024-dim) is far heavier than the previous all-MiniLM-L6-v2 default, so
/// 32 keeps peak memory bounded on machines with modest RAM while still
/// amortising per-call overhead.
const BATCH_SIZE: usize = 32;

/// Instruction prepended to *queries* (not documents) for instruction-tuned
/// embedding models such as Qwen3-Embedding.  The model was trained to embed
/// queries in the form `Instruct: {task}\nQuery: {text}`; documents are embedded
/// verbatim.  Using a code-retrieval-flavoured task description nudges the model
/// toward the right embedding subspace for this tool.
const QUERY_INSTRUCTION: &str =
    "Given a code search query, retrieve relevant code that matches the query";

/// How the per-token hidden states emitted by a `[batch, seq, hidden]` model are
/// reduced to a single vector per sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pooling {
    /// Attention-masked mean over all tokens — the convention for BERT-style
    /// sentence-transformers models (e.g. all-MiniLM-L6-v2).
    Mean,
    /// Hidden state of the last non-padded token — the convention for
    /// causal/decoder embedding models (e.g. Qwen3-Embedding).
    LastToken,
}

pub struct OrtEmbedding {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Tokenizer>,
    config: EmbeddingConfig,
    /// True when the loaded ONNX model declares `token_type_ids` as a required
    /// input (e.g. BERT-style models).  All-zero segment IDs are used because
    /// embedding models always encode a single sequence.
    needs_token_type_ids: bool,
    /// Pooling strategy applied to `[batch, seq, hidden]` outputs.
    pooling: Pooling,
    /// KV-cache model parameters, present when the ONNX export includes
    /// `past_key_values.*` inputs (e.g. onnx-community Qwen3-Embedding exports).
    /// For a single-pass embedding run, all past KV tensors are fed as empty
    /// (shape `[batch, n_heads, 0, head_dim]`), signalling no prior context.
    kv_cache: Option<KvCacheParams>,
}

/// Shape parameters for a decoder ONNX model's KV-cache inputs.
#[derive(Clone, Debug)]
struct KvCacheParams {
    /// Number of `past_key_values.N.{key,value}` input pairs.
    num_layers: usize,
    /// Number of KV heads per layer (from the model's input shape metadata).
    num_heads: usize,
    /// Per-head dimension (from the model's input shape metadata).
    head_dim: usize,
}

impl OrtEmbedding {
    /// Default HuggingFace model id used when no `--embedding-model` is given.
    /// Exposed so the DI container records the same model in namespace metadata
    /// that indexing actually uses, avoiding drift between the two.
    /// The onnx-community export is used because the upstream Qwen/Qwen3-Embedding-0.6B
    /// repo only ships model.safetensors — no ONNX export.
    pub(crate) const DEFAULT_MODEL_ID: &'static str =
        "onnx-community/Qwen3-Embedding-0.6B-ONNX";

    pub fn new(model_id: Option<&str>) -> Result<Self, DomainError> {
        let model_id = model_id.unwrap_or(Self::DEFAULT_MODEL_ID);
        debug!(
            "Initializing ORT embedding service with model: {}",
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

        // Pooling is inferred from the model architecture: BERT-style encoders
        // declare a `token_type_ids` (segment) input and are mean-pooled, while
        // causal/decoder embedding models (Qwen3-Embedding) have no segment
        // input and are pooled on their last token.
        let pooling = if needs_token_type_ids {
            Pooling::Mean
        } else {
            Pooling::LastToken
        };
        debug!("Embedding pooling strategy: {:?}", pooling);

        // Detect KV-cache decoder ONNX exports (e.g. onnx-community Qwen3-Embedding).
        // These have `past_key_values.0.key` inputs; for a single embedding pass we
        // feed all of them as empty tensors (seq_len=0), which tells the model there
        // is no cached context and it should attend only to the current tokens.
        let kv_cache = detect_kv_cache(&session);
        if let Some(ref kv) = kv_cache {
            debug!(
                "KV-cache decoder embedding model detected: {} layers, {} heads, head_dim={}",
                kv.num_layers, kv.num_heads, kv.head_dim
            );
        }

        let config = EmbeddingConfig::new(
            model_name.to_string(),
            DEFAULT_DIMENSIONS,
            DEFAULT_MAX_SEQ_LENGTH,
        );

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tokenizer: Arc::new(tokenizer),
            config,
            needs_token_type_ids,
            pooling,
            kv_cache,
        })
    }
}

/// Inspect an ONNX session's inputs and return KV-cache parameters if the model
/// declares `past_key_values.0.key` / `past_key_values.0.value` inputs.
/// Returns `None` for plain encoder models.
fn detect_kv_cache(session: &Session) -> Option<KvCacheParams> {
    let inputs = session.inputs();
    let num_layers = inputs
        .iter()
        .filter(|i| i.name().starts_with("past_key_values.") && i.name().ends_with(".key"))
        .count();
    if num_layers == 0 {
        return None;
    }
    // Read num_heads and head_dim from the first key input's shape annotation.
    // Shape is [batch, num_heads, past_seq, head_dim]; static dims are > 0.
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
    Some(KvCacheParams { num_layers, num_heads, head_dim })
}

/// Blocking (synchronous) embedding of a batch of texts.
///
/// Tokenisation and ONNX inference are both CPU-bound and blocking.
/// This function must only be called from a `tokio::task::spawn_blocking`
/// closure so that the tokio thread pool is not starved.
fn embed_texts_impl(
    session: &Mutex<Session>,
    tokenizer: &Tokenizer,
    max_seq_length: usize,
    needs_token_type_ids: bool,
    pooling: Pooling,
    kv_cache: Option<&KvCacheParams>,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, DomainError> {
    if texts.is_empty() {
        return Ok(vec![]);
    }

    let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let encodings = tokenizer
        .encode_batch(text_refs, true)
        .map_err(|e| DomainError::internal(format!("Tokenization failed: {}", e)))?;

    let batch_size = encodings.len();
    let max_len = encodings
        .iter()
        .map(|e| e.get_ids().len())
        .max()
        .unwrap_or(0)
        .min(max_seq_length);

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
    let input_ids_tensor = Tensor::from_array((shape, input_ids))
        .map_err(|e| DomainError::internal(format!("Failed to create input_ids tensor: {}", e)))?;
    let attention_mask_tensor = Tensor::from_array((shape, attention_mask)).map_err(|e| {
        DomainError::internal(format!("Failed to create attention_mask tensor: {}", e))
    })?;

    let mut session_guard = session
        .lock()
        .map_err(|e| DomainError::internal(format!("Failed to lock session: {}", e)))?;

    let outputs = if let Some(kv) = kv_cache {
        // Decoder / KV-cache model (e.g. onnx-community Qwen3-Embedding).
        // Supply position_ids = [0..max_len] and empty past_key_values so the
        // model performs a full-context forward pass with no cached state.
        // position_ids shape: [batch_size, max_len] — each row is [0, 1, ..., max_len-1].
        let pos_ids: Vec<i64> = (0..max_len as i64)
            .cycle()
            .take(batch_size * max_len)
            .collect();
        let pos_ids_tensor = Tensor::from_array(([batch_size, max_len], pos_ids))
            .map_err(|e| DomainError::internal(format!("Failed to create position_ids tensor: {}", e)))?;

        // Build the inputs map dynamically — ort::inputs![] requires a fixed
        // number of entries at compile time, so we use the HashMap path instead.
        let mut inputs_map: std::collections::HashMap<String, ort::value::Value> =
            std::collections::HashMap::new();
        inputs_map.insert("input_ids".to_string(), input_ids_tensor.into());
        inputs_map.insert("attention_mask".to_string(), attention_mask_tensor.into());
        inputs_map.insert("position_ids".to_string(), pos_ids_tensor.into());
        // Empty KV tensors: shape [batch, num_heads, 0, head_dim].  ndarray is
        // needed because ort rejects zero-dim tensors constructed from raw Vec.
        for layer in 0..kv.num_layers {
            let empty_k: Array4<f32> = Array4::zeros([batch_size, kv.num_heads, 0, kv.head_dim]);
            let empty_v: Array4<f32> = Array4::zeros([batch_size, kv.num_heads, 0, kv.head_dim]);
            inputs_map.insert(
                format!("past_key_values.{layer}.key"),
                Tensor::from_array(empty_k)
                    .map_err(|e| DomainError::internal(format!("Failed to create KV key {layer}: {e}")))?
                    .into(),
            );
            inputs_map.insert(
                format!("past_key_values.{layer}.value"),
                Tensor::from_array(empty_v)
                    .map_err(|e| DomainError::internal(format!("Failed to create KV value {layer}: {e}")))?
                    .into(),
            );
        }
        session_guard.run(inputs_map)
    } else if needs_token_type_ids {
        let token_type_ids = vec![0i64; batch_size * max_len];
        let token_type_ids_tensor = Tensor::from_array((shape, token_type_ids)).map_err(|e| {
            DomainError::internal(format!("Failed to create token_type_ids tensor: {}", e))
        })?;
        session_guard.run(ort::inputs![
            "input_ids" => input_ids_tensor,
            "attention_mask" => attention_mask_tensor,
            "token_type_ids" => token_type_ids_tensor,
        ])
    } else {
        session_guard.run(ort::inputs![
            "input_ids" => input_ids_tensor,
            "attention_mask" => attention_mask_tensor,
        ])
    }
    .map_err(|e| DomainError::internal(format!("Inference failed: {}", e)))?;

    let output_value = outputs
        .values()
        .next()
        .ok_or_else(|| DomainError::internal("No output tensor found"))?;

    let (shape, data) = output_value
        .try_extract_tensor::<f32>()
        .map_err(|e| DomainError::internal(format!("Failed to extract output tensor: {}", e)))?;

    let shape: Vec<usize> = shape.iter().map(|&x| x as usize).collect();
    debug!("Output tensor shape: {:?}", shape);

    let embeddings = if shape.len() == 3 {
        let hidden_size = shape[2];
        let seq_len = shape[1];

        (0..batch_size)
            .map(|i| {
                let mask = encodings[i].get_attention_mask();
                let mut embedding = match pooling {
                    Pooling::Mean => {
                        let mut acc = vec![0.0f32; hidden_size];
                        let mut count = 0.0f32;
                        for j in 0..seq_len.min(max_len) {
                            let mask_val = if j < mask.len() { mask[j] as f32 } else { 0.0 };
                            if mask_val > 0.0 {
                                for (k, emb_k) in acc.iter_mut().enumerate().take(hidden_size) {
                                    let idx = i * seq_len * hidden_size + j * hidden_size + k;
                                    *emb_k += data[idx] * mask_val;
                                }
                                count += mask_val;
                            }
                        }
                        if count > 0.0 {
                            for v in &mut acc {
                                *v /= count;
                            }
                        }
                        acc
                    }
                    Pooling::LastToken => {
                        // Index of the last attended token.  Inputs are
                        // right-padded, so this is `(number of real tokens) - 1`,
                        // clamped to the model's sequence length.
                        let real_len = mask.iter().take(max_len).filter(|&&m| m > 0).count();
                        let last = real_len.saturating_sub(1).min(seq_len.saturating_sub(1));
                        (0..hidden_size)
                            .map(|k| data[i * seq_len * hidden_size + last * hidden_size + k])
                            .collect()
                    }
                };

                let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for v in &mut embedding {
                        *v /= norm;
                    }
                }

                embedding
            })
            .collect()
    } else if shape.len() == 2 {
        let hidden_size = shape[1];

        (0..batch_size)
            .map(|i| {
                let mut embedding: Vec<f32> = (0..hidden_size)
                    .map(|j| data[i * hidden_size + j])
                    .collect();

                let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for v in &mut embedding {
                        *v /= norm;
                    }
                }

                embedding
            })
            .collect()
    } else {
        return Err(DomainError::internal(format!(
            "Unexpected output tensor shape: {:?}",
            shape
        )));
    };

    Ok(embeddings)
}

#[async_trait]
impl EmbeddingService for OrtEmbedding {
    async fn embed_chunk(&self, chunk: &CodeChunk) -> Result<Embedding, DomainError> {
        let text = format!(
            "{} {}",
            chunk.qualified_name().as_deref().unwrap_or(""),
            chunk.content()
        );
        let session = Arc::clone(&self.session);
        let tokenizer = Arc::clone(&self.tokenizer);
        let max_seq = self.config.max_sequence_length();
        let needs_tti = self.needs_token_type_ids;
        let pooling = self.pooling;
        let kv_cache = self.kv_cache.clone();
        let model_name = self.config.model_name().to_string();
        let chunk_id = chunk.id().to_string();

        let vectors = tokio::task::spawn_blocking(move || {
            embed_texts_impl(&session, &tokenizer, max_seq, needs_tti, pooling, kv_cache.as_ref(), &[text])
        })
        .await
        .map_err(|e| DomainError::internal(format!("Embedding task panicked: {e}")))??;

        Ok(Embedding::new(
            chunk_id,
            vectors.into_iter().next().unwrap_or_default(),
            model_name,
        ))
    }

    async fn embed_chunks(&self, chunks: &[CodeChunk]) -> Result<Vec<Embedding>, DomainError> {
        if chunks.is_empty() {
            return Ok(vec![]);
        }

        let model_name = self.config.model_name().to_string();
        let mut all_embeddings = Vec::with_capacity(chunks.len());

        for batch in chunks.chunks(BATCH_SIZE) {
            let texts: Vec<String> = batch
                .iter()
                .map(|c| {
                    format!(
                        "{} {}",
                        c.qualified_name().as_deref().unwrap_or(""),
                        c.content()
                    )
                })
                .collect();

            let session = Arc::clone(&self.session);
            let tokenizer = Arc::clone(&self.tokenizer);
            let max_seq = self.config.max_sequence_length();
            let needs_tti = self.needs_token_type_ids;
            let pooling = self.pooling;
            let kv_cache = self.kv_cache.clone();

            let vectors = tokio::task::spawn_blocking(move || {
                embed_texts_impl(&session, &tokenizer, max_seq, needs_tti, pooling, kv_cache.as_ref(), &texts)
            })
            .await
            .map_err(|e| DomainError::internal(format!("Embedding task panicked: {e}")))??;

            if vectors.len() != batch.len() {
                return Err(DomainError::internal(format!(
                    "embed_texts_impl returned {} vectors for {} chunks (model: {})",
                    vectors.len(),
                    batch.len(),
                    model_name,
                )));
            }

            for (chunk, vector) in batch.iter().zip(vectors) {
                all_embeddings.push(Embedding::new(
                    chunk.id().to_string(),
                    vector,
                    model_name.clone(),
                ));
            }
        }

        Ok(all_embeddings)
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError> {
        // Instruction-tuned (last-token) models expect queries wrapped in an
        // `Instruct: ...\nQuery: ...` template; documents are left bare so the
        // two sides of the retrieval pair stay in the trained format.
        let text = match self.pooling {
            Pooling::LastToken => format!("Instruct: {QUERY_INSTRUCTION}\nQuery: {query}"),
            Pooling::Mean => query.to_string(),
        };
        let session = Arc::clone(&self.session);
        let tokenizer = Arc::clone(&self.tokenizer);
        let max_seq = self.config.max_sequence_length();
        let needs_tti = self.needs_token_type_ids;
        let pooling = self.pooling;
        let kv_cache = self.kv_cache.clone();

        let vectors = tokio::task::spawn_blocking(move || {
            embed_texts_impl(&session, &tokenizer, max_seq, needs_tti, pooling, kv_cache.as_ref(), &[text])
        })
        .await
        .map_err(|e| DomainError::internal(format!("Embedding task panicked: {e}")))??;

        vectors
            .into_iter()
            .next()
            .ok_or_else(|| DomainError::internal("Failed to generate query embedding"))
    }

    fn config(&self) -> &EmbeddingConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "Requires model download"]
    async fn test_ort_embedding_service() {
        let service = OrtEmbedding::new(None).expect("Failed to create service");

        let embedding = service
            .embed_query("fn main() { println!(\"Hello\"); }")
            .await
            .unwrap();

        assert_eq!(embedding.len(), DEFAULT_DIMENSIONS);

        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "Embedding should be L2 normalized"
        );
    }
}
