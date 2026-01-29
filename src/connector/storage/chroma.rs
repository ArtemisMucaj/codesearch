//! ChromaDB vector store for persistent embedding storage.

use std::sync::Arc;

use async_trait::async_trait;
use chromadb::collection::{CollectionEntries, GetOptions, QueryOptions};
use chromadb::client::{ChromaAuthMethod, ChromaClient, ChromaClientOptions};
use chromadb::ChromaCollection;
use serde_json::Map;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::domain::{
    ChunkRepository, DomainError, Embedding, EmbeddingRepository, SearchQuery, SearchResult,
};

/// ChromaDB-based embedding storage for persistent vector search.
pub struct ChromaEmbeddingStorage {
    #[allow(dead_code)]
    client: ChromaClient,
    collection: Arc<Mutex<ChromaCollection>>,
    chunk_repo: Arc<dyn ChunkRepository>,
    #[allow(dead_code)]
    collection_name: String,
}

impl ChromaEmbeddingStorage {
    /// Create a new ChromaDB storage instance.
    ///
    /// # Arguments
    /// * `url` - ChromaDB server URL (e.g., "http://localhost:8000")
    /// * `collection_name` - Name of the collection to use
    /// * `chunk_repo` - Reference to the chunk repository for metadata lookups
    pub async fn new(
        url: &str,
        collection_name: &str,
        chunk_repo: Arc<dyn ChunkRepository>,
    ) -> Result<Self, DomainError> {
        let client = ChromaClient::new(ChromaClientOptions {
            url: Some(url.to_string()),
            database: "default_database".to_string(),
            auth: ChromaAuthMethod::None,
        })
        .await
        .map_err(|e| DomainError::internal(format!("Failed to connect to ChromaDB: {}", e)))?;

        info!("Connected to ChromaDB at {}", url);

        // Get or create the collection
        let collection = client
            .get_or_create_collection(collection_name, None)
            .await
            .map_err(|e| {
                DomainError::internal(format!("Failed to get/create collection: {}", e))
            })?;

        info!("Using ChromaDB collection: {}", collection_name);

        Ok(Self {
            client,
            collection: Arc::new(Mutex::new(collection)),
            chunk_repo,
            collection_name: collection_name.to_string(),
        })
    }

    /// Create with default local ChromaDB settings.
    #[allow(dead_code)]
    pub async fn new_local(
        collection_name: &str,
        chunk_repo: Arc<dyn ChunkRepository>,
    ) -> Result<Self, DomainError> {
        Self::new("http://localhost:8000", collection_name, chunk_repo).await
    }

    /// Create a metadata map from an embedding model name.
    fn create_metadata(model: &str) -> Map<String, serde_json::Value> {
        let mut map = Map::new();
        map.insert("model".to_string(), serde_json::Value::String(model.to_string()));
        map
    }
}

#[async_trait]
impl EmbeddingRepository for ChromaEmbeddingStorage {
    async fn save(&self, embedding: &Embedding) -> Result<(), DomainError> {
        let collection = self.collection.lock().await;

        let metadata = Self::create_metadata(&embedding.model);

        let entries = CollectionEntries {
            ids: vec![embedding.chunk_id.as_str()],
            embeddings: Some(vec![embedding.vector.clone()]),
            metadatas: Some(vec![metadata]),
            documents: None,
        };

        collection
            .upsert(entries, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to save embedding: {}", e)))?;

        debug!("Saved embedding for chunk: {}", embedding.chunk_id);
        Ok(())
    }

    async fn save_batch(&self, embeddings: &[Embedding]) -> Result<(), DomainError> {
        if embeddings.is_empty() {
            return Ok(());
        }

        let collection = self.collection.lock().await;

        let ids: Vec<&str> = embeddings.iter().map(|e| e.chunk_id.as_str()).collect();
        let vectors: Vec<Vec<f32>> = embeddings.iter().map(|e| e.vector.clone()).collect();
        let metadatas: Vec<Map<String, serde_json::Value>> = embeddings
            .iter()
            .map(|e| Self::create_metadata(&e.model))
            .collect();

        let entries = CollectionEntries {
            ids,
            embeddings: Some(vectors),
            metadatas: Some(metadatas),
            documents: None,
        };

        collection
            .upsert(entries, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to save embeddings batch: {}", e)))?;

        debug!("Saved {} embeddings to ChromaDB", embeddings.len());
        Ok(())
    }

    async fn find_by_chunk_id(&self, chunk_id: &str) -> Result<Option<Embedding>, DomainError> {
        let collection = self.collection.lock().await;

        let options = GetOptions {
            ids: vec![chunk_id.to_string()],
            where_metadata: None,
            limit: Some(1),
            offset: None,
            where_document: None,
            include: Some(vec!["embeddings".into(), "metadatas".into()]),
        };

        let result = collection
            .get(options)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to get embedding: {}", e)))?;

        if result.ids.is_empty() {
            return Ok(None);
        }

        // embeddings is Option<Vec<Option<Vec<f32>>>>
        let vector = result
            .embeddings
            .and_then(|e| e.into_iter().next()) // Option<Option<Vec<f32>>>
            .flatten() // Option<Vec<f32>>
            .unwrap_or_default();

        // metadatas is Option<Vec<Option<Map<String, Value>>>>
        let model = result
            .metadatas
            .and_then(|m| m.into_iter().next()) // Option<Option<Map>>
            .flatten() // Option<Map>
            .and_then(|m| {
                m.get("model")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "unknown".to_string());

        Ok(Some(Embedding::new(chunk_id.to_string(), vector, model)))
    }

    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError> {
        let collection = self.collection.lock().await;

        collection
            .delete(Some(vec![chunk_id]), None, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to delete embedding: {}", e)))?;

        debug!("Deleted embedding for chunk: {}", chunk_id);
        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        // Get all chunk IDs for this repository
        let chunks = self.chunk_repo.find_by_repository(repository_id).await?;

        if chunks.is_empty() {
            return Ok(());
        }

        let collection = self.collection.lock().await;
        let chunk_ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();

        // ChromaDB delete accepts a list of IDs
        collection
            .delete(Some(chunk_ids), None, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to delete embeddings: {}", e)))?;

        info!(
            "Deleted {} embeddings for repository: {}",
            chunks.len(),
            repository_id
        );
        Ok(())
    }

    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let collection = self.collection.lock().await;

        let query_options = QueryOptions {
            query_texts: None,
            query_embeddings: Some(vec![query_embedding.to_vec()]),
            where_metadata: None,
            where_document: None,
            n_results: Some(query.limit * 2), // Fetch extra to account for filtering
            include: Some(vec!["embeddings".into(), "distances".into()]),
        };

        let result = collection
            .query(query_options, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to search embeddings: {}", e)))?;

        // Release the lock before doing chunk lookups
        drop(collection);

        let mut search_results = Vec::new();

        // ChromaDB returns distances, we need to convert to similarity scores
        // For cosine distance: similarity = 1 - distance (if using cosine distance)
        // For L2 distance: similarity = 1 / (1 + distance)
        let ids = result.ids.into_iter().next().unwrap_or_default();
        let distances = result
            .distances
            .and_then(|d| d.into_iter().next())
            .unwrap_or_default();

        for (chunk_id, distance) in ids.into_iter().zip(distances.into_iter()) {
            // Convert distance to similarity score (assuming cosine distance)
            // ChromaDB uses L2 by default, but we'll normalize to 0-1 range
            let score = 1.0 / (1.0 + distance);

            // Apply minimum score filter
            if let Some(min_score) = query.min_score {
                if score < min_score {
                    continue;
                }
            }

            // Fetch the chunk metadata
            if let Some(chunk) = self.chunk_repo.find_by_id(&chunk_id).await? {
                // Apply language filter
                if let Some(ref languages) = query.languages {
                    if !languages.iter().any(|l| l == chunk.language.as_str()) {
                        continue;
                    }
                }

                // Apply node type filter
                if let Some(ref node_types) = query.node_types {
                    if !node_types.iter().any(|t| t == chunk.node_type.as_str()) {
                        continue;
                    }
                }

                // Apply repository filter
                if let Some(ref repo_ids) = query.repository_ids {
                    if !repo_ids.contains(&chunk.repository_id) {
                        continue;
                    }
                }

                search_results.push(SearchResult::new(chunk, score));

                // Stop if we have enough results
                if search_results.len() >= query.limit {
                    break;
                }
            }
        }

        Ok(search_results)
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let collection = self.collection.lock().await;

        let result = collection
            .count()
            .await
            .map_err(|e| DomainError::internal(format!("Failed to count embeddings: {}", e)))?;

        Ok(result as u64)
    }
}
