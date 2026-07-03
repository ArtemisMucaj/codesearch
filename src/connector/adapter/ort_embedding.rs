use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use tokenizers::Tokenizer;
use tracing::debug;

use crate::application::EmbeddingService;
use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

use crate::connector::adapter::DEFAULT_ONNX_EMBEDDING_MODEL as DEFAULT_MODEL_ID;

const DEFAULT_DIMENSIONS: usize = 384;
const DEFAULT_MAX_SEQ_LENGTH: usize = 512;
/// Number of chunks processed per ONNX inference call.
///
/// Larger batches amortise per-call overhead and improve CPU utilisation
/// via wider SIMD/matrix operations.  128 is a good default for the small
/// all-MiniLM-L6-v2 model (22M params, 384-dim) without risking OOM even
/// on machines with modest RAM.
const BATCH_SIZE: usize = 128;
/// Maximum number of `max_seq_length`-token windows embedded per text.
///
/// Texts longer than the model's context are split into consecutive token
/// windows whose vectors are combined (token-count-weighted mean, then L2
/// normalised) so that content past the context limit still contributes to
/// the embedding instead of being silently truncated.  The cap bounds
/// inference cost on pathological inputs (~4x context = 2048 tokens for the
/// default model); anything beyond it is truncated as before.
const MAX_EMBED_WINDOWS: usize = 4;

pub struct OrtEmbedding {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Tokenizer>,
    config: EmbeddingConfig,
    /// True when the loaded ONNX model declares `token_type_ids` as a required
    /// input (e.g. BERT-style models).  All-zero segment IDs are used because
    /// embedding models always encode a single sequence.
    needs_token_type_ids: bool,
}

impl OrtEmbedding {
    pub fn new(model_id: Option<&str>) -> Result<Self, DomainError> {
        let model_id = model_id.unwrap_or(DEFAULT_MODEL_ID);
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
        })
    }
}

/// A contiguous token range of one input text, embedded as its own batch row.
struct TokenWindow {
    /// Index of the source text in the input slice.
    owner: usize,
    /// Offset of the first token of this window.
    start: usize,
    /// Number of tokens in this window (<= max_seq_length).
    len: usize,
}

/// Blocking (synchronous) embedding of a batch of texts.
///
/// Tokenisation and ONNX inference are both CPU-bound and blocking.
/// This function must only be called from a `tokio::task::spawn_blocking`
/// closure so that the tokio thread pool is not starved.
///
/// Texts longer than `max_seq_length` are embedded as up to
/// [`MAX_EMBED_WINDOWS`] consecutive token windows whose pooled vectors are
/// combined by a token-count-weighted mean before the final L2 normalisation.
fn embed_texts_impl(
    session: &Mutex<Session>,
    tokenizer: &Tokenizer,
    max_seq_length: usize,
    needs_token_type_ids: bool,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, DomainError> {
    if texts.is_empty() {
        return Ok(vec![]);
    }

    let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let encodings = tokenizer
        .encode_batch(text_refs, true)
        .map_err(|e| DomainError::internal(format!("Tokenization failed: {}", e)))?;

    let mut windows: Vec<TokenWindow> = Vec::with_capacity(encodings.len());
    for (owner, encoding) in encodings.iter().enumerate() {
        let total = encoding.get_ids().len();
        if total == 0 {
            // Keep an all-padding row so every text still yields a vector.
            windows.push(TokenWindow {
                owner,
                start: 0,
                len: 0,
            });
            continue;
        }
        let mut start = 0;
        let mut window_count = 0;
        while start < total && window_count < MAX_EMBED_WINDOWS {
            let len = (total - start).min(max_seq_length);
            windows.push(TokenWindow { owner, start, len });
            start += len;
            window_count += 1;
        }
    }

    let batch_size = windows.len();
    let max_len = windows.iter().map(|w| w.len).max().unwrap_or(0).max(1);

    let mut input_ids: Vec<i64> = Vec::with_capacity(batch_size * max_len);
    let mut attention_mask: Vec<i64> = Vec::with_capacity(batch_size * max_len);

    for window in &windows {
        let encoding = &encodings[window.owner];
        let ids = &encoding.get_ids()[window.start..window.start + window.len];
        let mask = &encoding.get_attention_mask()[window.start..window.start + window.len];

        input_ids.extend(ids.iter().map(|&x| x as i64));
        attention_mask.extend(mask.iter().map(|&x| x as i64));

        let padding = max_len - window.len;
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

    let outputs = if needs_token_type_ids {
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

    // Pool each window row into an (unnormalised) vector plus the number of
    // attended tokens it covers, which serves as the combination weight.
    let (row_vectors, row_weights): (Vec<Vec<f32>>, Vec<f32>) = if shape.len() == 3 {
        let hidden_size = shape[2];
        let seq_len = shape[1];

        windows
            .iter()
            .enumerate()
            .map(|(i, window)| {
                let mut embedding = vec![0.0f32; hidden_size];
                let mut count = 0.0f32;

                let encoding = &encodings[window.owner];
                let mask = &encoding.get_attention_mask()[window.start..window.start + window.len];
                for (j, &mask_raw) in mask.iter().take(seq_len).enumerate() {
                    let mask_val = mask_raw as f32;
                    if mask_val > 0.0 {
                        for (k, emb_k) in embedding.iter_mut().enumerate().take(hidden_size) {
                            let idx = i * seq_len * hidden_size + j * hidden_size + k;
                            *emb_k += data[idx] * mask_val;
                        }
                        count += mask_val;
                    }
                }

                if count > 0.0 {
                    for v in &mut embedding {
                        *v /= count;
                    }
                }

                (embedding, count)
            })
            .unzip()
    } else if shape.len() == 2 {
        // The model pools internally: one vector per row.
        let hidden_size = shape[1];

        windows
            .iter()
            .enumerate()
            .map(|(i, window)| {
                let embedding: Vec<f32> = (0..hidden_size)
                    .map(|j| data[i * hidden_size + j])
                    .collect();
                let encoding = &encodings[window.owner];
                let attended: u32 = encoding.get_attention_mask()
                    [window.start..window.start + window.len]
                    .iter()
                    .sum();
                (embedding, attended as f32)
            })
            .unzip()
    } else {
        return Err(DomainError::internal(format!(
            "Unexpected output tensor shape: {:?}",
            shape
        )));
    };

    // Combine window vectors per source text (token-count-weighted mean),
    // then L2 normalise once.  For the common single-window case this is
    // exactly the previous mean-pool + normalise behaviour.
    let hidden_size = row_vectors.first().map(|v| v.len()).unwrap_or(0);
    let mut embeddings: Vec<Vec<f32>> = vec![vec![0.0f32; hidden_size]; encodings.len()];
    let mut weights: Vec<f32> = vec![0.0f32; encodings.len()];

    for (row, window) in windows.iter().enumerate() {
        let weight = row_weights[row];
        if weight <= 0.0 {
            continue;
        }
        let acc = &mut embeddings[window.owner];
        for (a, v) in acc.iter_mut().zip(&row_vectors[row]) {
            *a += v * weight;
        }
        weights[window.owner] += weight;
    }

    for (embedding, weight) in embeddings.iter_mut().zip(&weights) {
        if *weight > 0.0 {
            for v in embedding.iter_mut() {
                *v /= *weight;
            }
        }
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in embedding.iter_mut() {
                *v /= norm;
            }
        }
    }

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
        let model_name = self.config.model_name().to_string();
        let chunk_id = chunk.id().to_string();

        let vectors = tokio::task::spawn_blocking(move || {
            embed_texts_impl(&session, &tokenizer, max_seq, needs_tti, &[text])
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

        // Each inference batch is padded to its longest member, so mixing
        // short and long chunks wastes compute on padding tokens.  Process
        // batches in ascending text-length order (byte length is a good token
        // proxy) and scatter results back into the original chunk order.
        let mut texts: Vec<String> = chunks
            .iter()
            .map(|c| {
                format!(
                    "{} {}",
                    c.qualified_name().as_deref().unwrap_or(""),
                    c.content()
                )
            })
            .collect();
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());

        let mut vectors: Vec<Option<Vec<f32>>> = vec![None; chunks.len()];

        for batch_indices in order.chunks(BATCH_SIZE) {
            let batch_texts: Vec<String> = batch_indices
                .iter()
                .map(|&i| std::mem::take(&mut texts[i]))
                .collect();

            let session = Arc::clone(&self.session);
            let tokenizer = Arc::clone(&self.tokenizer);
            let max_seq = self.config.max_sequence_length();
            let needs_tti = self.needs_token_type_ids;

            let batch_vectors = tokio::task::spawn_blocking(move || {
                embed_texts_impl(&session, &tokenizer, max_seq, needs_tti, &batch_texts)
            })
            .await
            .map_err(|e| DomainError::internal(format!("Embedding task panicked: {e}")))??;

            if batch_vectors.len() != batch_indices.len() {
                return Err(DomainError::internal(format!(
                    "embed_texts_impl returned {} vectors for {} chunks (model: {})",
                    batch_vectors.len(),
                    batch_indices.len(),
                    model_name,
                )));
            }

            for (&i, vector) in batch_indices.iter().zip(batch_vectors) {
                vectors[i] = Some(vector);
            }
        }

        let mut all_embeddings = Vec::with_capacity(chunks.len());
        for (chunk, vector) in chunks.iter().zip(vectors) {
            let vector = vector.ok_or_else(|| {
                DomainError::internal(format!("Missing embedding for chunk {}", chunk.id()))
            })?;
            all_embeddings.push(Embedding::new(
                chunk.id().to_string(),
                vector,
                model_name.clone(),
            ));
        }

        Ok(all_embeddings)
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError> {
        let text = query.to_string();
        let session = Arc::clone(&self.session);
        let tokenizer = Arc::clone(&self.tokenizer);
        let max_seq = self.config.max_sequence_length();
        let needs_tti = self.needs_token_type_ids;

        let vectors = tokio::task::spawn_blocking(move || {
            embed_texts_impl(&session, &tokenizer, max_seq, needs_tti, &[text])
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
