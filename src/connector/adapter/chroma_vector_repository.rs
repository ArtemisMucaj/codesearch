use std::sync::Arc;

use async_trait::async_trait;
use chromadb::client::{ChromaAuthMethod, ChromaClient, ChromaClientOptions};
use chromadb::collection::{CollectionEntries, GetOptions, QueryOptions};
use chromadb::ChromaCollection;
use serde_json::Map;
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::VectorRepository;
use crate::domain::{CodeChunk, DomainError, Embedding, SearchQuery, SearchResult};

pub struct ChromaVectorRepository {
    #[allow(dead_code)]
    client: ChromaClient,
    collection: Arc<Mutex<ChromaCollection>>,
}

impl ChromaVectorRepository {
    pub async fn new(url: &str, collection_name: &str) -> Result<Self, DomainError> {
        let client = ChromaClient::new(ChromaClientOptions {
            url: Some(url.to_string()),
            database: "default_database".to_string(),
            auth: ChromaAuthMethod::None,
        })
        .await
        .map_err(|e| DomainError::internal(format!("Failed to connect to ChromaDB: {}", e)))?;

        debug!("Connected to ChromaDB at {}", url);

        let collection = client
            .get_or_create_collection(collection_name, None)
            .await
            .map_err(|e| {
                DomainError::internal(format!("Failed to get/create collection: {}", e))
            })?;

        debug!("Using ChromaDB collection: {}", collection_name);

        Ok(Self {
            client,
            collection: Arc::new(Mutex::new(collection)),
        })
    }

    #[allow(dead_code)]
    pub async fn new_local(collection_name: &str) -> Result<Self, DomainError> {
        Self::new("http://localhost:8000", collection_name).await
    }

    fn create_metadata(chunk: &CodeChunk, model: &str) -> Map<String, serde_json::Value> {
        let mut map = Map::new();
        map.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
        map.insert(
            "file_path".to_string(),
            serde_json::Value::String(chunk.file_path().to_string()),
        );
        map.insert(
            "start_line".to_string(),
            serde_json::Value::Number((chunk.start_line() as u64).into()),
        );
        map.insert(
            "end_line".to_string(),
            serde_json::Value::Number((chunk.end_line() as u64).into()),
        );
        map.insert(
            "language".to_string(),
            serde_json::Value::String(chunk.language().as_str().to_string()),
        );
        map.insert(
            "node_type".to_string(),
            serde_json::Value::String(chunk.node_type().as_str().to_string()),
        );
        map.insert(
            "repository_id".to_string(),
            serde_json::Value::String(chunk.repository_id().to_string()),
        );
        if let Some(name) = chunk.symbol_name() {
            map.insert(
                "symbol_name".to_string(),
                serde_json::Value::String(name.to_string()),
            );
        }
        if let Some(parent) = chunk.parent_symbol() {
            map.insert(
                "parent_symbol".to_string(),
                serde_json::Value::String(parent.to_string()),
            );
        }
        map
    }

    fn chunk_from_metadata(
        id: &str,
        content: String,
        metadata: &Map<String, serde_json::Value>,
    ) -> Result<CodeChunk, DomainError> {
        let file_path = metadata
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DomainError::internal("Missing file_path metadata"))?
            .to_string();
        let start_line = metadata
            .get("start_line")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| DomainError::internal("Missing start_line metadata"))?
            as u32;
        let end_line = metadata
            .get("end_line")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| DomainError::internal("Missing end_line metadata"))?
            as u32;
        let language_str = metadata
            .get("language")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let node_type_str = metadata
            .get("node_type")
            .and_then(|v| v.as_str())
            .unwrap_or("block");
        let repository_id = metadata
            .get("repository_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DomainError::internal("Missing repository_id metadata"))?
            .to_string();

        let language = crate::domain::Language::parse(language_str);
        let node_type = crate::domain::NodeType::parse(node_type_str);

        let chunk = CodeChunk::reconstitute(
            id.to_string(),
            file_path,
            content,
            start_line,
            end_line,
            language,
            node_type,
            metadata
                .get("symbol_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            metadata
                .get("parent_symbol")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            repository_id,
        );

        Ok(chunk)
    }
}

#[async_trait]
impl VectorRepository for ChromaVectorRepository {
    async fn save_batch(
        &self,
        chunks: &[CodeChunk],
        embeddings: &[Embedding],
    ) -> Result<(), DomainError> {
        if chunks.is_empty() {
            return Ok(());
        }

        if chunks.len() != embeddings.len() {
            return Err(DomainError::InvalidInput(
                "Chunk and embedding count mismatch".to_string(),
            ));
        }

        let collection = self.collection.lock().await;

        let ids: Vec<&str> = chunks.iter().map(|chunk| chunk.id()).collect();
        let documents: Vec<&str> = chunks.iter().map(|chunk| chunk.content()).collect();
        let vectors: Vec<Vec<f32>> = embeddings.iter().map(|e| e.vector().to_vec()).collect();
        let metadatas: Vec<Map<String, serde_json::Value>> = chunks
            .iter()
            .zip(embeddings.iter())
            .map(|(chunk, embedding)| Self::create_metadata(chunk, embedding.model()))
            .collect();

        let entries = CollectionEntries {
            ids,
            embeddings: Some(vectors),
            metadatas: Some(metadatas),
            documents: Some(documents),
        };

        collection
            .upsert(entries, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to save batch: {}", e)))?;

        debug!("Saved {} chunks to ChromaDB", chunks.len());
        Ok(())
    }

    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError> {
        let collection = self.collection.lock().await;
        collection
            .delete(Some(vec![chunk_id]), None, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to delete chunk: {}", e)))?;
        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let collection = self.collection.lock().await;
        let where_metadata = serde_json::json!({"repository_id": repository_id});
        let ids = collection
            .get(GetOptions {
                ids: vec![],
                where_metadata: Some(where_metadata),
                limit: None,
                offset: None,
                where_document: None,
                include: None,
            })
            .await
            .map_err(|e| DomainError::internal(format!("Failed to fetch ids: {}", e)))?
            .ids;

        if ids.is_empty() {
            return Ok(());
        }

        let id_refs: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
        collection
            .delete(Some(id_refs), None, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to delete chunks: {}", e)))?;
        Ok(())
    }

    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError> {
        let collection = self.collection.lock().await;
        let where_metadata = serde_json::json!({
            "$and": [
                {"repository_id": repository_id},
                {"file_path": file_path}
            ]
        });
        let ids = collection
            .get(GetOptions {
                ids: vec![],
                where_metadata: Some(where_metadata),
                limit: None,
                offset: None,
                where_document: None,
                include: None,
            })
            .await
            .map_err(|e| DomainError::internal(format!("Failed to fetch ids: {}", e)))?
            .ids;

        let count = ids.len() as u64;
        if ids.is_empty() {
            return Ok(0);
        }

        let id_refs: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
        collection
            .delete(Some(id_refs), None, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to delete chunks: {}", e)))?;

        debug!(
            "Deleted {} chunks for file {} in repository {}",
            count, file_path, repository_id
        );
        Ok(count)
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
            n_results: Some(query.limit() * 2),
            include: Some(vec!["distances", "metadatas", "documents"]),
        };

        let result = collection
            .query(query_options, None)
            .await
            .map_err(|e| DomainError::internal(format!("Failed to search embeddings: {}", e)))?;

        let ids = result.ids.into_iter().next().unwrap_or_default();
        let distances = result
            .distances
            .and_then(|d| d.into_iter().next())
            .unwrap_or_default();
        let metadatas = result
            .metadatas
            .and_then(|m| m.into_iter().next())
            .unwrap_or_default();
        let documents = result
            .documents
            .and_then(|d| d.into_iter().next())
            .unwrap_or_default();

        let mut search_results = Vec::new();

        for ((chunk_id, distance), (metadata_opt, document)) in ids
            .into_iter()
            .zip(distances.into_iter())
            .zip(metadatas.into_iter().zip(documents.into_iter()))
        {
            let score = 1.0 / (1.0 + distance);

            if let Some(min_score) = query.min_score() {
                if score < min_score {
                    continue;
                }
            }

            let metadata = match metadata_opt {
                Some(metadata) => metadata,
                None => continue,
            };

            let chunk = Self::chunk_from_metadata(&chunk_id, document, &metadata)?;

            if let Some(languages) = query.languages() {
                if !languages.iter().any(|l| l == chunk.language().as_str()) {
                    continue;
                }
            }

            if let Some(node_types) = query.node_types() {
                if !node_types.iter().any(|t| t == chunk.node_type().as_str()) {
                    continue;
                }
            }

            if let Some(repo_ids) = query.repository_ids() {
                if !repo_ids.contains(&chunk.repository_id().to_string()) {
                    continue;
                }
            }

            search_results.push(SearchResult::new(chunk, score));

            if search_results.len() >= query.limit() {
                break;
            }
        }

        Ok(search_results)
    }

    /// ChromaDB does not expose SQL-level text search; hybrid mode degrades
    /// gracefully to semantic-only when this backend is in use.
    async fn search_text(
        &self,
        _terms: &[&str],
        _query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        Ok(vec![])
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let collection = self.collection.lock().await;
        let result = collection
            .count()
            .await
            .map_err(|e| DomainError::internal(format!("Failed to count chunks: {}", e)))?;
        Ok(result as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Language, NodeType};

    #[test]
    fn test_chroma_metadata_roundtrip() {
        let chunk = CodeChunk::new(
            "src/lib.rs".to_string(),
            "fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
            1,
            1,
            Language::Rust,
            NodeType::Function,
            "repo".to_string(),
        )
        .with_symbol_name("add");

        let embedding = Embedding::new(chunk.id().to_string(), vec![0.0; 3], "mock".to_string());
        let metadata = ChromaVectorRepository::create_metadata(&chunk, embedding.model());
        let rebuilt = ChromaVectorRepository::chunk_from_metadata(
            chunk.id(),
            chunk.content().to_string(),
            &metadata,
        )
        .expect("chunk rebuild should succeed");

        assert_eq!(rebuilt.file_path(), chunk.file_path());
        assert_eq!(rebuilt.language(), chunk.language());
        assert_eq!(rebuilt.node_type(), chunk.node_type());
    }
}
