use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::array::{Float32Array, StringArray, UInt32Array};
use arrow_array::FixedSizeListArray;
use arrow_schema::{DataType, Field, Schema};
use arrow_array::RecordBatch;
use arrow_array::RecordBatchIterator;
use async_trait::async_trait;
use futures::stream::TryStreamExt;
use lancedb::DistanceType;
use lancedb::query::ExecutableQuery;
use tracing::{debug, error};

use crate::application::VectorRepository;
use crate::domain::{CodeChunk, DomainError, Embedding, NodeType, SearchQuery, SearchResult};

pub struct LanceDbVectorRepository {
    db_path: PathBuf,
    table_name: String,
}

impl LanceDbVectorRepository {
    pub async fn new(data_dir: &Path, table_name: &str) -> Result<Self, DomainError> {
        // Create lancedb directory if it doesn't exist
        let db_path = data_dir.join("lancedb");
        tokio::fs::create_dir_all(&db_path)
            .await
            .map_err(|e| {
                DomainError::storage(format!(
                    "Failed to create LanceDB directory at {}: {}",
                    db_path.display(),
                    e
                ))
            })?;

        let repo = Self {
            db_path,
            table_name: table_name.to_string(),
        };

        // Verify database can be opened
        let _ = lancedb::connect(repo.db_path.to_string_lossy().as_ref())
            .execute()
            .await
            .map_err(|e| {
                DomainError::storage(format!("Failed to connect to LanceDB: {}", e))
            })?;

        debug!(
            "LanceDB vector repository initialized at {:?}",
            repo.db_path
        );

        Ok(repo)
    }

    fn create_schema(embedding_dim: usize) -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("chunk_id", DataType::Utf8, false),
            Field::new("file_path", DataType::Utf8, false),
            Field::new("content", DataType::Utf8, false),
            Field::new("start_line", DataType::UInt32, false),
            Field::new("end_line", DataType::UInt32, false),
            Field::new("language", DataType::Utf8, false),
            Field::new("node_type", DataType::Utf8, false),
            Field::new("symbol_name", DataType::Utf8, true),
            Field::new("parent_symbol", DataType::Utf8, true),
            Field::new("repository_id", DataType::Utf8, false),
            Field::new(
                "embedding",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    embedding_dim as i32,
                ),
                false,
            ),
            Field::new("model", DataType::Utf8, false),
        ]))
    }

    fn chunks_to_record_batch(
        chunks: &[CodeChunk],
        embeddings: &[Embedding],
    ) -> Result<RecordBatch, DomainError> {
        if chunks.is_empty() {
            return Err(DomainError::storage(
                "Cannot create RecordBatch from empty chunks".to_string(),
            ));
        }

        let embedding_map: std::collections::HashMap<String, &Embedding> = embeddings
            .iter()
            .map(|e| (e.chunk_id().to_string(), e))
            .collect();

        // Get embedding dimension from first embedding
        let embedding_dim = embeddings.first()
            .ok_or_else(|| DomainError::storage("No embeddings provided".to_string()))?
            .vector()
            .len();

        let schema = Self::create_schema(embedding_dim);
        let mut chunk_ids = Vec::new();
        let mut file_paths = Vec::new();
        let mut contents = Vec::new();
        let mut start_lines = Vec::new();
        let mut end_lines = Vec::new();
        let mut languages = Vec::new();
        let mut node_types = Vec::new();
        let mut symbol_names = Vec::new();
        let mut parent_symbols = Vec::new();
        let mut repository_ids = Vec::new();
        let mut all_embeddings = Vec::new();
        let mut models = Vec::new();

        for chunk in chunks {
            let embedding = embedding_map.get(chunk.id()).ok_or_else(|| {
                DomainError::storage(format!(
                    "No embedding found for chunk {}",
                    chunk.id()
                ))
            })?;

            chunk_ids.push(chunk.id().to_string());
            file_paths.push(chunk.file_path().to_string());
            contents.push(chunk.content().to_string());
            start_lines.push(chunk.start_line());
            end_lines.push(chunk.end_line());
            languages.push(chunk.language().to_string());
            node_types.push(chunk.node_type().as_str().to_string());
            symbol_names.push(chunk.symbol_name().map(|s| s.to_string()));
            parent_symbols.push(chunk.parent_symbol().map(|s| s.to_string()));
            repository_ids.push(chunk.repository_id().to_string());
            all_embeddings.push(embedding.vector().to_vec());
            models.push(embedding.model().to_string());
        }

        // Create Arrow arrays
        let chunk_id_array = Arc::new(StringArray::from(chunk_ids));
        let file_path_array = Arc::new(StringArray::from(file_paths));
        let content_array = Arc::new(StringArray::from(contents));
        let start_line_array = Arc::new(UInt32Array::from_iter_values(start_lines));
        let end_line_array = Arc::new(UInt32Array::from_iter_values(end_lines));
        let language_array = Arc::new(StringArray::from(languages));
        let node_type_array = Arc::new(StringArray::from(node_types));
        let symbol_name_array = Arc::new(StringArray::from(symbol_names));
        let parent_symbol_array = Arc::new(StringArray::from(parent_symbols));
        let repository_id_array = Arc::new(StringArray::from(repository_ids));
        let model_array = Arc::new(StringArray::from(models));

        // Create embedding array (FixedSizeList of Float32)
        let embedding_vectors: Vec<Option<Vec<Option<f32>>>> = all_embeddings
            .iter()
            .map(|vec| Some(vec.iter().map(|&f| Some(f)).collect()))
            .collect();

        let embedding_array = Arc::new(
            FixedSizeListArray::from_iter_primitive::<arrow_array::types::Float32Type, _, _>(
                embedding_vectors,
                embedding_dim as i32,
            ),
        );

        RecordBatch::try_new(
            schema,
            vec![
                chunk_id_array,
                file_path_array,
                content_array,
                start_line_array,
                end_line_array,
                language_array,
                node_type_array,
                symbol_name_array,
                parent_symbol_array,
                repository_id_array,
                embedding_array,
                model_array,
            ],
        )
        .map_err(|e| {
            DomainError::storage(format!("Failed to create RecordBatch: {}", e))
        })
    }
}

#[async_trait]
impl VectorRepository for LanceDbVectorRepository {
    async fn save_batch(
        &self,
        chunks: &[CodeChunk],
        embeddings: &[Embedding],
    ) -> Result<(), DomainError> {
        if chunks.is_empty() || embeddings.is_empty() {
            return Ok(());
        }

        if chunks.len() != embeddings.len() {
            return Err(DomainError::storage(
                "Number of chunks and embeddings must match".to_string(),
            ));
        }

        let record_batch = Self::chunks_to_record_batch(chunks, embeddings)?;

        let db = lancedb::connect(self.db_path.to_string_lossy().as_ref())
            .execute()
            .await
            .map_err(|e| {
                DomainError::storage(format!("Failed to connect to LanceDB: {}", e))
            })?;

        // Check if table exists
        let table_exists = db
            .table_names()
            .execute()
            .await
            .ok()
            .map(|names| names.contains(&self.table_name))
            .unwrap_or(false);

        let schema = record_batch.schema();
        let batches = RecordBatchIterator::new(
            vec![Ok(record_batch)].into_iter(),
            schema.clone(),
        );

        if table_exists {
            // Add to existing table
            let table = db
                .open_table(&self.table_name)
                .execute()
                .await
                .map_err(|e| {
                    DomainError::storage(format!("Failed to open LanceDB table: {}", e))
                })?;

            table
                .add(batches)
                .execute()
                .await
                .map_err(|e| {
                    error!("Failed to add records to LanceDB: {}", e);
                    DomainError::storage(format!("Failed to save vectors to LanceDB: {}", e))
                })?;
        } else {
            // Create new table
            db.create_table(&self.table_name, batches)
                .execute()
                .await
                .map_err(|e| {
                    DomainError::storage(format!("Failed to create LanceDB table: {}", e))
                })?;
        }

        debug!("Saved {} chunks to LanceDB", chunks.len());
        Ok(())
    }

    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError> {
        let db = lancedb::connect(self.db_path.to_string_lossy().as_ref())
            .execute()
            .await
            .map_err(|e| {
                DomainError::storage(format!("Failed to connect to LanceDB: {}", e))
            })?;

        if let Ok(table) = db
            .open_table(&self.table_name)
            .execute()
            .await
        {
            table
                .delete(&format!("chunk_id = '{}'", chunk_id))
                .await
                .map_err(|e| {
                    error!("Failed to delete chunk {} from LanceDB: {}", chunk_id, e);
                    DomainError::storage(format!(
                        "Failed to delete chunk from LanceDB: {}",
                        e
                    ))
                })?;

            debug!("Deleted chunk {} from LanceDB", chunk_id);
        }

        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let db = lancedb::connect(self.db_path.to_string_lossy().as_ref())
            .execute()
            .await
            .map_err(|e| {
                DomainError::storage(format!("Failed to connect to LanceDB: {}", e))
            })?;

        if let Ok(table) = db
            .open_table(&self.table_name)
            .execute()
            .await
        {
            table
                .delete(&format!("repository_id = '{}'", repository_id))
                .await
                .map_err(|e| {
                    error!(
                        "Failed to delete chunks from repository {} in LanceDB: {}",
                        repository_id, e
                    );
                    DomainError::storage(format!(
                        "Failed to delete repository chunks from LanceDB: {}",
                        e
                    ))
                })?;

            debug!("Deleted all chunks for repository {} from LanceDB", repository_id);
        }

        Ok(())
    }

    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let db = lancedb::connect(self.db_path.to_string_lossy().as_ref())
            .execute()
            .await
            .map_err(|e| {
                DomainError::storage(format!("Failed to connect to LanceDB: {}", e))
            })?;

        let table = db
            .open_table(&self.table_name)
            .execute()
            .await
            .map_err(|e| {
                DomainError::storage(format!("Failed to open LanceDB table: {}", e))
            })?;

        let query_builder = table
            .query()
            .nearest_to(query_embedding.to_vec())
            .map_err(|e| {
                error!("Failed to build search query: {}", e);
                DomainError::storage(format!("Failed to build search query: {}", e))
            })?
            .distance_type(DistanceType::Cosine);

        // Execute the vector search query
        let record_batches = query_builder
            .execute()
            .await
            .map_err(|e| {
                error!("Failed to search LanceDB: {}", e);
                DomainError::storage(format!("Failed to search vectors in LanceDB: {}", e))
            })?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| {
                error!("Failed to collect search results: {}", e);
                DomainError::storage(format!("Failed to collect search results: {}", e))
            })?;

        let mut final_results = Vec::new();

        // Process each RecordBatch
        for batch in record_batches {
            if final_results.len() >= query.limit() {
                break;
            }

            let num_rows = batch.num_rows();

            // Get column indices
            let chunk_id_col = batch.column_by_name("chunk_id")
                .ok_or_else(|| DomainError::storage("Missing chunk_id column".to_string()))?;
            let file_path_col = batch.column_by_name("file_path")
                .ok_or_else(|| DomainError::storage("Missing file_path column".to_string()))?;
            let content_col = batch.column_by_name("content")
                .ok_or_else(|| DomainError::storage("Missing content column".to_string()))?;
            let start_line_col = batch.column_by_name("start_line")
                .ok_or_else(|| DomainError::storage("Missing start_line column".to_string()))?;
            let end_line_col = batch.column_by_name("end_line")
                .ok_or_else(|| DomainError::storage("Missing end_line column".to_string()))?;
            let language_col = batch.column_by_name("language")
                .ok_or_else(|| DomainError::storage("Missing language column".to_string()))?;
            let node_type_col = batch.column_by_name("node_type")
                .ok_or_else(|| DomainError::storage("Missing node_type column".to_string()))?;
            let symbol_name_col = batch.column_by_name("symbol_name")
                .ok_or_else(|| DomainError::storage("Missing symbol_name column".to_string()))?;
            let parent_symbol_col = batch.column_by_name("parent_symbol")
                .ok_or_else(|| DomainError::storage("Missing parent_symbol column".to_string()))?;
            let repository_id_col = batch.column_by_name("repository_id")
                .ok_or_else(|| DomainError::storage("Missing repository_id column".to_string()))?;
            let distance_col = batch.column_by_name("_distance")
                .ok_or_else(|| DomainError::storage("Missing _distance column".to_string()))?;

            for i in 0..num_rows {
                if final_results.len() >= query.limit() {
                    break;
                }

                // Extract fields from columns - need to downcast from Arc<dyn Array>
                let chunk_id_str_col = chunk_id_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid chunk_id type".to_string()))?;
                let chunk_id = chunk_id_str_col.value(i).to_string();

                let file_path_str_col = file_path_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid file_path type".to_string()))?;
                let file_path = file_path_str_col.value(i).to_string();

                let content_str_col = content_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid content type".to_string()))?;
                let content = content_str_col.value(i).to_string();

                let start_line_uint_col = start_line_col.as_any().downcast_ref::<UInt32Array>()
                    .ok_or_else(|| DomainError::storage("Invalid start_line type".to_string()))?;
                let start_line = start_line_uint_col.value(i);

                let end_line_uint_col = end_line_col.as_any().downcast_ref::<UInt32Array>()
                    .ok_or_else(|| DomainError::storage("Invalid end_line type".to_string()))?;
                let end_line = end_line_uint_col.value(i);

                let language_str_col = language_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid language type".to_string()))?;
                let language_str = language_str_col.value(i);
                let language = crate::domain::Language::parse(language_str);

                let node_type_str_col = node_type_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid node_type type".to_string()))?;
                let node_type_str = node_type_str_col.value(i);

                let symbol_name_str_col = symbol_name_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid symbol_name type".to_string()))?;
                let symbol_name = if symbol_name_col.is_null(i) {
                    None
                } else {
                    Some(symbol_name_str_col.value(i).to_string())
                };

                let parent_symbol_str_col = parent_symbol_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid parent_symbol type".to_string()))?;
                let parent_symbol = if parent_symbol_col.is_null(i) {
                    None
                } else {
                    Some(parent_symbol_str_col.value(i).to_string())
                };

                let repository_id_str_col = repository_id_col.as_any().downcast_ref::<StringArray>()
                    .ok_or_else(|| DomainError::storage("Invalid repository_id type".to_string()))?;
                let repository_id = repository_id_str_col.value(i).to_string();

                // Get score from distance column
                let distance_col_casted = distance_col.as_any()
                    .downcast_ref::<Float32Array>()
                    .ok_or_else(|| DomainError::storage("Invalid _distance type".to_string()))?;
                let score = distance_col_casted.value(i);

                // Apply filters
                if let Some(min_score) = query.min_score() {
                    if score < min_score {
                        continue;
                    }
                }

                if let Some(languages) = query.languages() {
                    if !languages.iter().any(|l| l == language_str) {
                        continue;
                    }
                }

                if let Some(node_types) = query.node_types() {
                    if !node_types.iter().any(|t| t == node_type_str) {
                        continue;
                    }
                }

                if let Some(repo_ids) = query.repository_ids() {
                    if !repo_ids.contains(&repository_id) {
                        continue;
                    }
                }

                // Reconstruct CodeChunk
                let chunk = CodeChunk::reconstitute(
                    chunk_id,
                    file_path,
                    content,
                    start_line,
                    end_line,
                    language,
                    NodeType::parse(node_type_str),
                    symbol_name,
                    parent_symbol,
                    repository_id,
                );

                final_results.push(SearchResult::new(chunk, score));
            }
        }

        Ok(final_results)
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let db = lancedb::connect(self.db_path.to_string_lossy().as_ref())
            .execute()
            .await
            .map_err(|e| {
                DomainError::storage(format!("Failed to connect to LanceDB: {}", e))
            })?;

        if let Ok(table) = db
            .open_table(&self.table_name)
            .execute()
            .await
        {
            let count = table.count_rows(None).await.map_err(|e| {
                DomainError::storage(format!("Failed to count rows in LanceDB: {}", e))
            })? as u64;
            Ok(count)
        } else {
            Ok(0)
        }
    }
}
