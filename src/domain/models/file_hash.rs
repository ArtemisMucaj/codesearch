use serde::{Deserialize, Serialize};

/// Represents a file's content hash for incremental indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHash {
    file_path: String,
    content_hash: String,
    repository_id: String,
}

impl FileHash {
    pub fn new(file_path: String, content_hash: String, repository_id: String) -> Self {
        Self {
            file_path,
            content_hash,
            repository_id,
        }
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }
}

/// Computes SHA-256 hash of file content.
pub fn compute_file_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(content.as_bytes());
    format!("{:x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_hash_creation() {
        let hash = FileHash::new(
            "src/main.rs".to_string(),
            "abc123".to_string(),
            "repo-1".to_string(),
        );

        assert_eq!(hash.file_path(), "src/main.rs");
        assert_eq!(hash.content_hash(), "abc123");
        assert_eq!(hash.repository_id(), "repo-1");
    }

    #[test]
    fn test_compute_file_hash() {
        let content = "fn main() {}";
        let hash = compute_file_hash(content);

        // SHA-256 produces a 64-character hex string
        assert_eq!(hash.len(), 64);

        // Same content should produce same hash
        let hash2 = compute_file_hash(content);
        assert_eq!(hash, hash2);

        // Different content should produce different hash
        let hash3 = compute_file_hash("fn main() { println!(\"hello\"); }");
        assert_ne!(hash, hash3);
    }
}
