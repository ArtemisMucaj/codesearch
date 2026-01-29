use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
    chunk_id: String,
    vector: Vec<f32>,
    model: String,
}

impl Embedding {
    pub fn new(chunk_id: String, vector: Vec<f32>, model: String) -> Self {
        Self {
            chunk_id,
            vector,
            model,
        }
    }

    pub fn chunk_id(&self) -> &str {
        &self.chunk_id
    }

    pub fn vector(&self) -> &[f32] {
        &self.vector
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn dimensions(&self) -> usize {
        self.vector.len()
    }

    pub fn is_normalized(&self) -> bool {
        let magnitude = self.magnitude();
        (magnitude - 1.0).abs() < 0.01
    }

    pub fn magnitude(&self) -> f32 {
        self.vector.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    pub fn normalized(&self) -> Self {
        let mag = self.magnitude();
        let normalized_vector = if mag > 0.0 {
            self.vector.iter().map(|x| x / mag).collect()
        } else {
            self.vector.clone()
        };

        Self {
            chunk_id: self.chunk_id.clone(),
            vector: normalized_vector,
            model: self.model.clone(),
        }
    }

    pub fn cosine_similarity(&self, other: &Embedding) -> f32 {
        if self.vector.len() != other.vector.len() {
            return 0.0;
        }

        let dot: f32 = self
            .vector
            .iter()
            .zip(other.vector.iter())
            .map(|(a, b)| a * b)
            .sum();

        let norm_self = self.magnitude();
        let norm_other = other.magnitude();

        if norm_self == 0.0 || norm_other == 0.0 {
            0.0
        } else {
            dot / (norm_self * norm_other)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    model_name: String,
    dimensions: usize,
    max_sequence_length: usize,
}

impl EmbeddingConfig {
    pub fn new(model_name: String, dimensions: usize, max_sequence_length: usize) -> Self {
        Self {
            model_name,
            dimensions,
            max_sequence_length,
        }
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub fn max_sequence_length(&self) -> usize {
        self.max_sequence_length
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_creation() {
        let embedding = Embedding::new(
            "chunk-1".to_string(),
            vec![0.5, 0.5, 0.5, 0.5],
            "test-model".to_string(),
        );

        assert_eq!(embedding.chunk_id(), "chunk-1");
        assert_eq!(embedding.dimensions(), 4);
        assert_eq!(embedding.model(), "test-model");
    }

    #[test]
    fn test_magnitude() {
        let embedding = Embedding::new("chunk".to_string(), vec![3.0, 4.0], "test".to_string());

        assert!((embedding.magnitude() - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_normalization() {
        let embedding = Embedding::new("chunk".to_string(), vec![3.0, 4.0], "test".to_string());

        let normalized = embedding.normalized();
        assert!(normalized.is_normalized());
    }

    #[test]
    fn test_cosine_similarity() {
        let e1 = Embedding::new("a".to_string(), vec![1.0, 0.0], "m".to_string());
        let e2 = Embedding::new("b".to_string(), vec![1.0, 0.0], "m".to_string());
        let e3 = Embedding::new("c".to_string(), vec![0.0, 1.0], "m".to_string());

        // Same vectors should have similarity ~1.0
        assert!((e1.cosine_similarity(&e2) - 1.0).abs() < 0.001);

        // Orthogonal vectors should have similarity ~0.0
        assert!((e1.cosine_similarity(&e3)).abs() < 0.001);
    }
}
