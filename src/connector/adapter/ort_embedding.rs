use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use tokenizers::Tokenizer;
use tracing::{debug, info};

use crate::application::EmbeddingService;
use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

const DEFAULT_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
const DEFAULT_DIMENSIONS: usize = 384;
const DEFAULT_MAX_SEQ_LENGTH: usize = 256;

pub struct OrtEmbedding {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Tokenizer>,
    config: EmbeddingConfig,
}

impl OrtEmbedding {
    pub fn new(model_id: Option<&str>) -> Result<Self, DomainError> {
        let model_id = model_id.unwrap_or(DEFAULT_MODEL_ID);
        info!("Initializing ORT embedding service with model: {}", model_id);

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

        let session = Session::builder()
            .map_err(|e| DomainError::internal(format!("Failed to create session builder: {}", e)))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| DomainError::internal(format!("Failed to set optimization level: {}", e)))?
            .commit_from_file(&model_path)
            .map_err(|e| DomainError::internal(format!("Failed to load ONNX model: {}", e)))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| DomainError::internal(format!("Failed to load tokenizer: {}", e)))?;

        let config = EmbeddingConfig::new(
            model_name.to_string(),
            DEFAULT_DIMENSIONS,
            DEFAULT_MAX_SEQ_LENGTH,
        );

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tokenizer: Arc::new(tokenizer),
            config,
        })
    }

    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| DomainError::internal(format!("Tokenization failed: {}", e)))?;

        let batch_size = encodings.len();
        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .min(self.config.max_sequence_length());

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
        let input_ids_tensor = Tensor::from_array((shape, input_ids))
            .map_err(|e| DomainError::internal(format!("Failed to create input_ids tensor: {}", e)))?;
        let attention_mask_tensor = Tensor::from_array((shape, attention_mask))
            .map_err(|e| DomainError::internal(format!("Failed to create attention_mask tensor: {}", e)))?;
        let token_type_ids_tensor = Tensor::from_array((shape, token_type_ids))
            .map_err(|e| DomainError::internal(format!("Failed to create token_type_ids tensor: {}", e)))?;

        let mut session = self
            .session
            .lock()
            .map_err(|e| DomainError::internal(format!("Failed to lock session: {}", e)))?;

        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            ])
            .map_err(|e| DomainError::internal(format!("Inference failed: {}", e)))?;

        let output_value = outputs
            .iter()
            .next()
            .map(|(_, v)| v)
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
                    let mut embedding = vec![0.0f32; hidden_size];
                    let mut count = 0.0f32;

                    let mask = encodings[i].get_attention_mask();
                    for j in 0..seq_len.min(max_len) {
                        let mask_val = if j < mask.len() { mask[j] as f32 } else { 0.0 };
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
}

#[async_trait]
impl EmbeddingService for OrtEmbedding {
    async fn embed_chunk(&self, chunk: &CodeChunk) -> Result<Embedding, DomainError> {
        let text = format!("{} {}", chunk.symbol_name().unwrap_or(""), chunk.content());
        let vectors = self.embed_texts(&[&text])?;

        Ok(Embedding::new(
            chunk.id().to_string(),
            vectors.into_iter().next().unwrap_or_default(),
            self.config.model_name().to_string(),
        ))
    }

    async fn embed_chunks(&self, chunks: &[CodeChunk]) -> Result<Vec<Embedding>, DomainError> {
        if chunks.is_empty() {
            return Ok(vec![]);
        }

        const BATCH_SIZE: usize = 32;
        let mut all_embeddings = Vec::with_capacity(chunks.len());

        for batch in chunks.chunks(BATCH_SIZE) {
            let texts: Vec<String> = batch
                .iter()
                .map(|c| format!("{} {}", c.symbol_name().unwrap_or(""), c.content()))
                .collect();
            let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();

            let vectors = self.embed_texts(&text_refs)?;

            for (chunk, vector) in batch.iter().zip(vectors) {
                all_embeddings.push(Embedding::new(
                    chunk.id().to_string(),
                    vector,
                    self.config.model_name().to_string(),
                ));
            }
        }

        Ok(all_embeddings)
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError> {
        let vectors = self.embed_texts(&[query])?;
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

        let embedding = service.embed_query("fn main() { println!(\"Hello\"); }").await.unwrap();

        assert_eq!(embedding.len(), DEFAULT_DIMENSIONS);

        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01);
    }
}
