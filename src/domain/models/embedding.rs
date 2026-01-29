use serde::{Deserialize, Serialize};

/// Represents a vector embedding for a code chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
    pub chunk_id: String,
    pub vector: Vec<f32>,
    pub model: String,
}

impl Embedding {
    pub fn new(chunk_id: String, vector: Vec<f32>, model: String) -> Self {
        Self {
            chunk_id,
            vector,
            model,
        }
    }

    pub fn dimensions(&self) -> usize {
        self.vector.len()
    }
}

/// Configuration for the embedding model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub model_name: String,
    pub dimensions: usize,
    pub max_sequence_length: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model_name: "mock-embedding".to_string(),
            dimensions: 384,
            max_sequence_length: 512,
        }
    }
}
