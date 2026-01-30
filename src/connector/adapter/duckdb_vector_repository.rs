use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::VectorRepository;
use crate::domain::{CodeChunk, DomainError, Embedding, SearchQuery, SearchResult};

const VECTOR_DIMENSIONS: usize = 384;

pub struct DuckdbVectorRepository {
    conn: Arc<Mutex<Connection>>,
    namespace: String,
}

impl DuckdbVectorRepository {
    pub fn new(path: &Path) -> Result<Self, DomainError> {
        Self::new_with_namespace(path, "main")
    }

    pub fn new_with_namespace(path: &Path, namespace: &str) -> Result<Self, DomainError> {
        let conn = Connection::open(path)
            .map_err(|e| DomainError::storage(format!("Failed to open DuckDB database: {}", e)))?;
        Self::initialize(&conn, namespace)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: namespace.to_string(),
        })
    }

    #[allow(dead_code)]
    pub fn in_memory() -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| DomainError::storage(format!("Failed to open DuckDB in-memory DB: {}", e)))?;
        let namespace = "main";
        Self::initialize(&conn, namespace)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: namespace.to_string(),
        })
    }

    /// Returns a clone of the shared connection Arc.
    /// This allows other adapters to share the same DuckDB connection,
    /// which is necessary because DuckDB only allows one write connection per file.
    pub fn shared_connection(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    /// Initializes tables and enables VSS extension.
    fn initialize(conn: &Connection, schema: &str) -> Result<(), DomainError> {
        let schema = schema.trim();
        let schema_name = if schema.is_empty() { "main" } else { schema };
        debug!("Initializing DuckDB with schema: {}", schema_name);
        let create_schema = format!("CREATE SCHEMA IF NOT EXISTS \"{}\";", schema_name);
        if let Err(e) = conn.execute_batch(&create_schema) {
            return Err(DomainError::storage(format!(
                "Failed to create DuckDB schema {}: {}",
                schema_name, e
            )));
        }

        let create_tables = format!(
            "\
            CREATE TABLE IF NOT EXISTS \"{}\".chunks (
                id TEXT PRIMARY KEY,
                file_path TEXT NOT NULL,
                content TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                language TEXT NOT NULL,
                node_type TEXT NOT NULL,
                symbol_name TEXT,
                parent_symbol TEXT,
                repository_id TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS \"{}\".embeddings (
                chunk_id TEXT PRIMARY KEY,
                vector FLOAT[384] NOT NULL,
                model TEXT NOT NULL
            );
            ",
            schema_name, schema_name
        );

        if let Err(e) = conn.execute_batch(&create_tables) {
            return Err(DomainError::storage(format!(
                "Failed to initialize DuckDB tables: {}",
                e
            )));
        }
        debug!("DuckDB tables created successfully in schema {}", schema_name);

        // Create repositories table in main schema (shared with MetadataRepository)
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS repositories (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                chunk_count BIGINT DEFAULT 0,
                file_count BIGINT DEFAULT 0,
                store TEXT DEFAULT 'duckdb',
                namespace TEXT
            );
            "#,
        )
        .map_err(|e| DomainError::storage(format!("Failed to create repositories table: {}", e)))?;
        debug!("Repositories table created successfully");

        // VSS extension is required for vector search
        conn.execute_batch("INSTALL vss;")
            .map_err(|e| DomainError::storage(format!("Failed to INSTALL vss: {}", e)))?;
        conn.execute_batch("LOAD vss;")
            .map_err(|e| DomainError::storage(format!("Failed to LOAD vss: {}", e)))?;

        conn.execute_batch("SET hnsw_enable_experimental_persistence = true;")
            .map_err(|e| DomainError::storage(format!("Failed to set HNSW persistence: {}", e)))?;

        conn.execute_batch(
            &format!(
                "CREATE INDEX IF NOT EXISTS embedding_hnsw_idx ON \"{}\".embeddings USING HNSW (vector) WITH (metric = 'cosine');",
                schema_name
            ),
        )
        .map_err(|e| DomainError::storage(format!("Failed to create HNSW index: {}", e)))?;

        Ok(())
    }

    fn vector_to_array_literal(vector: &[f32]) -> Result<String, DomainError> {
        if vector.len() != VECTOR_DIMENSIONS {
            return Err(DomainError::invalid_input(format!(
                "Expected embedding dimension {}, got {}",
                VECTOR_DIMENSIONS,
                vector.len()
            )));
        }
        let mut s = String::with_capacity(vector.len() * 8);
        s.push('[');
        for (i, v) in vector.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            // DuckDB accepts standard float literals.
            s.push_str(&format!("{}", v));
        }
        s.push(']');
        s.push_str("::FLOAT[384]");
        Ok(s)
    }

}

#[async_trait]
impl VectorRepository for DuckdbVectorRepository {
    async fn save_batch(
        &self,
        chunks: &[CodeChunk],
        embeddings: &[Embedding],
    ) -> Result<(), DomainError> {
        if chunks.is_empty() {
            return Ok(());
        }
        if chunks.len() != embeddings.len() {
            return Err(DomainError::invalid_input(
                "Chunk and embedding count mismatch".to_string(),
            ));
        }

        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        {
            let mut stmt = tx
                .prepare(
                    &format!(
                        "INSERT OR REPLACE INTO \"{}\".chunks \
                        (id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id) \
                        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        self.namespace
                    ),
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare chunk insert: {}", e)))?;

            for chunk in chunks {
                stmt.execute(params![
                    chunk.id(),
                    chunk.file_path(),
                    chunk.content(),
                    chunk.start_line() as i64,
                    chunk.end_line() as i64,
                    chunk.language().as_str(),
                    chunk.node_type().as_str(),
                    chunk.symbol_name(),
                    chunk.parent_symbol(),
                    chunk.repository_id(),
                ])
                .map_err(|e| {
                    DomainError::storage(format!("Failed to insert chunk {}: {}", chunk.id(), e))
                })?;
            }
        }

        for embedding in embeddings {
            let array_lit = Self::vector_to_array_literal(embedding.vector())?;
            // Note: The array literal must be part of the SQL statement (not parameterized)
            // because DuckDB FLOAT[384] type doesn't support parameterization.
            // This is safe since the array is constructed from our embedding data, not user input.
            let sql = format!(
                "INSERT OR REPLACE INTO \"{}\".embeddings (chunk_id, vector, model) \
                VALUES (?, {}, ?)",
                self.namespace, array_lit
            );
            tx.execute(&sql, params![embedding.chunk_id(), embedding.model()])
                .map_err(|e| {
                    DomainError::storage(format!(
                        "Failed to insert embedding for chunk {}: {}",
                        embedding.chunk_id(),
                        e
                    ))
                })?;
        }

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!(
            "Saved {} chunks and {} embeddings to DuckDB",
            chunks.len(),
            embeddings.len()
        );
        Ok(())
    }

    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError> {
        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;
        tx.execute(
            &format!("DELETE FROM \"{}\".embeddings WHERE chunk_id = ?", self.namespace),
            params![chunk_id],
        )
            .map_err(|e| DomainError::storage(format!("Failed to delete embedding: {}", e)))?;
        tx.execute(
            &format!("DELETE FROM \"{}\".chunks WHERE id = ?", self.namespace),
            params![chunk_id],
        )
            .map_err(|e| DomainError::storage(format!("Failed to delete chunk: {}", e)))?;
        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;
        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        tx.execute(
            &format!(
                "DELETE FROM \"{0}\".embeddings WHERE chunk_id IN (SELECT id FROM \"{0}\".chunks WHERE repository_id = ?)",
                self.namespace
            ),
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete embeddings: {}", e)))?;

        tx.execute(
            &format!("DELETE FROM \"{}\".chunks WHERE repository_id = ?", self.namespace),
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete chunks: {}", e)))?;

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;
        Ok(())
    }

    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        if query_embedding.len() != VECTOR_DIMENSIONS {
            return Err(DomainError::invalid_input(format!(
                "Expected query embedding dimension {}, got {}",
                VECTOR_DIMENSIONS,
                query_embedding.len()
            )));
        }

        let array_lit = Self::vector_to_array_literal(query_embedding)?;
        let mut sql = format!(
            "SELECT \
                c.id, c.file_path, c.content, c.start_line, c.end_line, c.language, c.node_type, c.symbol_name, c.parent_symbol, c.repository_id, \
                1.0 - array_cosine_distance(e.vector, {array_lit}) AS score \
            FROM \"{schema}\".embeddings e \
            JOIN \"{schema}\".chunks c ON c.id = e.chunk_id \
            ",
            array_lit = array_lit,
            schema = self.namespace
        );

        let mut where_clauses: Vec<String> = Vec::new();
        if let Some(languages) = query.languages() {
            let quoted = languages
                .iter()
                .map(|l| format!("'{}'", l.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            where_clauses.push(format!("c.language IN ({})", quoted));
        }
        if let Some(node_types) = query.node_types() {
            let quoted = node_types
                .iter()
                .map(|t| format!("'{}'", t.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            where_clauses.push(format!("c.node_type IN ({})", quoted));
        }
        if let Some(repo_ids) = query.repository_ids() {
            let quoted = repo_ids
                .iter()
                .map(|r| format!("'{}'", r.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            where_clauses.push(format!("c.repository_id IN ({})", quoted));
        }
        if !where_clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_clauses.join(" AND "));
        }

        sql.push_str(&format!(
            " ORDER BY array_cosine_distance(e.vector, {array_lit}) LIMIT ?",
            array_lit = array_lit
        ));

        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare search: {}", e)))?;
        let mut rows = stmt
            .query(params![query.limit() as i64])
            .map_err(|e| DomainError::storage(format!("Failed to run search: {}", e)))?;

        let mut results = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?
        {
            let score: f32 = row
                .get(10)
                .map_err(|e| DomainError::storage(format!("Failed to read score: {}", e)))?;

            if let Some(min_score) = query.min_score() {
                if score < min_score {
                    continue;
                }
            }

            let chunk = CodeChunk::reconstitute(
                row.get::<_, String>(0)
                    .map_err(|e| DomainError::storage(format!("Failed to read id: {}", e)))?,
                row.get::<_, String>(1)
                    .map_err(|e| DomainError::storage(format!("Failed to read file_path: {}", e)))?,
                row.get::<_, String>(2)
                    .map_err(|e| DomainError::storage(format!("Failed to read content: {}", e)))?,
                row.get::<_, i64>(3)
                    .map_err(|e| DomainError::storage(format!("Failed to read start_line: {}", e)))? as u32,
                row.get::<_, i64>(4)
                    .map_err(|e| DomainError::storage(format!("Failed to read end_line: {}", e)))? as u32,
                crate::domain::Language::parse(
                    &row.get::<_, String>(5)
                        .map_err(|e| DomainError::storage(format!("Failed to read language: {}", e)))?,
                ),
                crate::domain::NodeType::parse(
                    &row.get::<_, String>(6)
                        .map_err(|e| DomainError::storage(format!("Failed to read node_type: {}", e)))?,
                ),
                row.get::<_, Option<String>>(7)
                    .map_err(|e| DomainError::storage(format!("Failed to read symbol_name: {}", e)))?,
                row.get::<_, Option<String>>(8)
                    .map_err(|e| DomainError::storage(format!("Failed to read parent_symbol: {}", e)))?,
                row.get::<_, String>(9)
                    .map_err(|e| DomainError::storage(format!("Failed to read repository_id: {}", e)))?,
            );

            results.push(SearchResult::new(chunk, score));
            if results.len() >= query.limit() {
                break;
            }
        }
        Ok(results)
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{}\".chunks", self.namespace),
                [],
                |row| row.get(0),
            )
            .map_err(|e| DomainError::storage(format!("Failed to count chunks: {}", e)))?;
        Ok(count as u64)
    }
}
