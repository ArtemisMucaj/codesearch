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
        cosine_similarity(&self.vector, &other.vector)
    }
}

/// Cosine similarity of two raw vectors; `0.0` for mismatched lengths, empty
/// input, or a zero vector.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut norm_a, mut norm_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
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

    #[test]
    fn raw_cosine_similarity_edge_cases() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[0.0, 0.0]), 0.0);
    }
}
