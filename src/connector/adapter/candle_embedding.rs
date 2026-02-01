use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};
use tokenizers::Tokenizer;
use tracing::{debug, info};

use crate::application::EmbeddingService;
use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

const DEFAULT_MODEL_ID: &str = "mixedbread-ai/mxbai-embed-xsmall-v1";
const DEFAULT_MAX_SEQ_LENGTH: usize = 512;
const BATCH_SIZE: usize = 32;

pub struct CandleEmbedding {
    model: Mutex<BertModel>,
    tokenizer: Tokenizer,
    device: Device,
    config: EmbeddingConfig,
}

impl CandleEmbedding {
    /// Create a new CandleEmbedding service using the specified model or the default.
    /// Set `use_gpu` to true to attempt GPU acceleration (CUDA or Metal).
    pub fn new(model_id: Option<&str>, use_gpu: bool) -> Result<Self, DomainError> {
        let model_id = model_id.unwrap_or(DEFAULT_MODEL_ID);
        info!(
            "Initializing Candle embedding service with model: {} (GPU: {})",
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
        info!("Loading Candle model from: {:?}", weights_path);

        // Load config
        let config_content = std::fs::read_to_string(&config_path)
            .map_err(|e| DomainError::internal(format!("Failed to read config: {}", e)))?;
        let bert_config: BertConfig = serde_json::from_str(&config_content)
            .map_err(|e| DomainError::internal(format!("Failed to parse config: {}", e)))?;

        // Load weights
        let vb = if weights_path
            .extension()
            .map_or(false, |e| e == "safetensors")
        {
            unsafe {
                VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device).map_err(
                    |e| DomainError::internal(format!("Failed to load safetensors: {}", e)),
                )?
            }
        } else {
            // For .bin files (PyTorch format)
            VarBuilder::from_pth(&weights_path, DTYPE, &device).map_err(|e| {
                DomainError::internal(format!("Failed to load PyTorch weights: {}", e))
            })?
        };

        // Build model
        let model = BertModel::load(vb, &bert_config)
            .map_err(|e| DomainError::internal(format!("Failed to build BERT model: {}", e)))?;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| DomainError::internal(format!("Failed to load tokenizer: {}", e)))?;

        let embedding_dim = bert_config.hidden_size;

        let config = EmbeddingConfig::new(
            model_name.to_string(),
            embedding_dim,
            DEFAULT_MAX_SEQ_LENGTH,
        );

        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            device,
            config,
        })
    }

    fn select_device(use_gpu: bool) -> Result<Device, DomainError> {
        if use_gpu {
            // Try CUDA first
            #[cfg(feature = "cuda")]
            {
                if let Ok(device) = Device::new_cuda(0) {
                    info!("CUDA device available, using GPU");
                    return Ok(device);
                }
            }

            // Try Metal (macOS)
            #[cfg(feature = "metal")]
            {
                if let Ok(device) = Device::new_metal(0) {
                    info!("Metal device available, using GPU");
                    return Ok(device);
                }
            }

            info!("No GPU available, falling back to CPU");
        }

        Ok(Device::Cpu)
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

        // Build input tensors
        let mut input_ids_vec: Vec<u32> = Vec::with_capacity(batch_size * max_len);
        let mut attention_mask_vec: Vec<u32> = Vec::with_capacity(batch_size * max_len);
        let mut token_type_ids_vec: Vec<u32> = Vec::with_capacity(batch_size * max_len);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();
            let type_ids = encoding.get_type_ids();

            let len = ids.len().min(max_len);

            input_ids_vec.extend(ids[..len].iter());
            attention_mask_vec.extend(mask[..len].iter());
            token_type_ids_vec.extend(type_ids[..len].iter());

            // Padding
            let padding = max_len - len;
            input_ids_vec.extend(std::iter::repeat_n(0u32, padding));
            attention_mask_vec.extend(std::iter::repeat_n(0u32, padding));
            token_type_ids_vec.extend(std::iter::repeat_n(0u32, padding));
        }

        // Create tensors
        let input_ids = Tensor::from_vec(input_ids_vec, (batch_size, max_len), &self.device)
            .map_err(|e| {
                DomainError::internal(format!("Failed to create input_ids tensor: {}", e))
            })?;

        let attention_mask = Tensor::from_vec(
            attention_mask_vec.clone(),
            (batch_size, max_len),
            &self.device,
        )
        .map_err(|e| {
            DomainError::internal(format!("Failed to create attention_mask tensor: {}", e))
        })?;

        let token_type_ids =
            Tensor::from_vec(token_type_ids_vec, (batch_size, max_len), &self.device).map_err(
                |e| DomainError::internal(format!("Failed to create token_type_ids tensor: {}", e)),
            )?;

        // Run forward pass
        let model = self
            .model
            .lock()
            .map_err(|e| DomainError::internal(format!("Failed to lock model: {}", e)))?;

        let output = model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| DomainError::internal(format!("Inference failed: {}", e)))?;

        debug!("Output tensor shape: {:?}", output.shape());

        // Mean pooling over sequence dimension, weighted by attention mask
        let embeddings =
            self.mean_pool_and_normalize(&output, &attention_mask_vec, batch_size, max_len)?;

        Ok(embeddings)
    }

    fn mean_pool_and_normalize(
        &self,
        output: &Tensor,
        attention_mask: &[u32],
        batch_size: usize,
        seq_len: usize,
    ) -> Result<Vec<Vec<f32>>, DomainError> {
        let hidden_size = self.config.dimensions();

        // Convert output tensor to Vec<f32>
        let output_data: Vec<f32> = output
            .to_dtype(DType::F32)
            .map_err(|e| DomainError::internal(format!("Failed to convert dtype: {}", e)))?
            .flatten_all()
            .map_err(|e| DomainError::internal(format!("Failed to flatten tensor: {}", e)))?
            .to_vec1()
            .map_err(|e| DomainError::internal(format!("Failed to convert to vec: {}", e)))?;

        let mut embeddings = Vec::with_capacity(batch_size);

        for i in 0..batch_size {
            let mut embedding = vec![0.0f32; hidden_size];
            let mut count = 0.0f32;

            for j in 0..seq_len {
                let mask_val = attention_mask[i * seq_len + j] as f32;
                if mask_val > 0.0 {
                    for k in 0..hidden_size {
                        let idx = i * seq_len * hidden_size + j * hidden_size + k;
                        embedding[k] += output_data[idx] * mask_val;
                    }
                    count += mask_val;
                }
            }

            // Average by token count
            if count > 0.0 {
                for v in &mut embedding {
                    *v /= count;
                }
            }

            // L2 normalize
            let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in &mut embedding {
                    *v /= norm;
                }
            }

            embeddings.push(embedding);
        }

        Ok(embeddings)
    }
}

#[async_trait]
impl EmbeddingService for CandleEmbedding {
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

// Expose the device for testing
impl CandleEmbedding {
    pub fn device(&self) -> &Device {
        &self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "Requires model download"]
    async fn test_candle_embedding_service() {
        let service = CandleEmbedding::new(None, false).expect("Failed to create service");
        let expected_dims = service.config().dimensions();

        let embedding = service
            .embed_query("fn main() { println!(\"Hello\"); }")
            .await
            .unwrap();

        assert_eq!(embedding.len(), expected_dims);

        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "Embedding should be L2 normalized"
        );
    }

    #[tokio::test]
    #[ignore = "Requires model download"]
    async fn test_candle_embedding_batch() {
        let service = CandleEmbedding::new(None, false).expect("Failed to create service");
        let expected_dims = service.config().dimensions();

        let texts = vec!["fn add(a: i32, b: i32) -> i32 { a + b }"; 5];
        let embeddings = service.embed_texts(&texts).unwrap();

        assert_eq!(embeddings.len(), 5);
        for emb in &embeddings {
            assert_eq!(emb.len(), expected_dims);
        }
    }
}
