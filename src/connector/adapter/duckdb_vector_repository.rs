use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection, Row};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::{rrf_fuse, VectorRepository};
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
        let conn = Connection::open_in_memory().map_err(|e| {
            DomainError::storage(format!("Failed to open DuckDB in-memory DB: {}", e))
        })?;
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

        // Install and load VSS extension first (required for vector type)
        conn.execute_batch("INSTALL vss; LOAD vss; SET hnsw_enable_experimental_persistence = true;")
            .map_err(|e| DomainError::storage(format!("Failed to initialize VSS extension: {}", e)))?;

        // Create all tables in a single batch
        let schema_sql = format!(
            r#"
            CREATE SCHEMA IF NOT EXISTS "{}";

            CREATE TABLE IF NOT EXISTS "{}".chunks (
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

            CREATE TABLE IF NOT EXISTS "{}".embeddings (
                chunk_id TEXT PRIMARY KEY,
                vector FLOAT[384] NOT NULL,
                model TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS repositories (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                chunk_count BIGINT DEFAULT 0,
                file_count BIGINT DEFAULT 0,
                store TEXT DEFAULT 'duckdb',
                namespace TEXT,
                languages TEXT
            );

            CREATE INDEX IF NOT EXISTS embedding_hnsw_idx ON "{}".embeddings USING HNSW (vector) WITH (metric = 'cosine');
            "#,
            schema_name, schema_name, schema_name, schema_name
        );

        conn.execute_batch(&schema_sql)
            .map_err(|e| DomainError::storage(format!("Failed to initialize DuckDB schema: {}", e)))?;

        debug!("DuckDB schema initialized successfully");
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

    // ── Private query helpers (synchronous, take &Connection) ────────────────

    fn row_to_chunk(row: &Row) -> Result<CodeChunk, duckdb::Error> {
        Ok(CodeChunk::reconstitute(
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            u32::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
            u32::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
            crate::domain::Language::parse(&row.get::<_, String>(5)?),
            crate::domain::NodeType::parse(&row.get::<_, String>(6)?),
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, String>(9)?,
        ))
    }

    fn run_semantic(
        conn: &Connection,
        namespace: &str,
        array_lit: &str,
        query: &SearchQuery,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let mut sql = format!(
            "SELECT \
                c.id, c.file_path, c.content, c.start_line, c.end_line, c.language, c.node_type, \
                c.symbol_name, c.parent_symbol, c.repository_id, \
                1.0 - array_cosine_distance(e.vector, {array_lit}) AS score \
             FROM \"{schema}\".embeddings e \
             JOIN \"{schema}\".chunks c ON c.id = e.chunk_id",
            array_lit = array_lit,
            schema = namespace,
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

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare semantic search: {}", e)))?;
        let mut rows = stmt
            .query(params![limit as i64])
            .map_err(|e| DomainError::storage(format!("Failed to run semantic search: {}", e)))?;

        let mut results = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read semantic row: {}", e)))?
        {
            let score: f32 = row
                .get(10)
                .map_err(|e| DomainError::storage(format!("Failed to read score: {}", e)))?;
            if let Some(min) = query.min_score() {
                if score < min {
                    continue;
                }
            }
            let chunk = Self::row_to_chunk(row)
                .map_err(|e| DomainError::storage(format!("Failed to parse chunk row: {}", e)))?;
            results.push(SearchResult::new(chunk, score));
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    fn run_text(
        conn: &Connection,
        namespace: &str,
        terms: &[&str],
        query: &SearchQuery,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DomainError> {
        if terms.is_empty() {
            return Ok(vec![]);
        }

        let max_score = (terms.len() * 3) as f64;
        let mut score_parts: Vec<String> = Vec::new();
        let mut where_parts: Vec<String> = Vec::new();

        for term in terms {
            let safe = term.to_lowercase()
                .replace('\\', "\\\\")
                .replace('\'', "''")
                .replace('%', "\\%")
                .replace('_', "\\_");
            score_parts.push(format!(
                "(CASE WHEN LOWER(c.content) LIKE '%{s}%' ESCAPE '\\\\' THEN 1.0 ELSE 0.0 END \
                 + CASE WHEN LOWER(c.symbol_name) LIKE '%{s}%' ESCAPE '\\\\' THEN 2.0 ELSE 0.0 END)",
                s = safe
            ));
            where_parts.push(format!(
                "LOWER(c.content) LIKE '%{s}%' ESCAPE '\\\\' OR LOWER(c.symbol_name) LIKE '%{s}%' ESCAPE '\\\\'",
                s = safe
            ));
        }

        let score_expr = format!("({}) / {:.1}", score_parts.join(" + "), max_score);
        let where_expr = where_parts.join(" OR ");

        let mut sql = format!(
            "SELECT c.id, c.file_path, c.content, c.start_line, c.end_line, \
             c.language, c.node_type, c.symbol_name, c.parent_symbol, c.repository_id, \
             CAST({score} AS FLOAT) AS score \
             FROM \"{ns}\".chunks c \
             WHERE ({where_clause})",
            score = score_expr,
            ns = namespace,
            where_clause = where_expr,
        );

        let mut extra: Vec<String> = Vec::new();
        if let Some(languages) = query.languages() {
            let quoted = languages
                .iter()
                .map(|l| format!("'{}'", l.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            extra.push(format!("c.language IN ({})", quoted));
        }
        if let Some(node_types) = query.node_types() {
            let quoted = node_types
                .iter()
                .map(|t| format!("'{}'", t.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            extra.push(format!("c.node_type IN ({})", quoted));
        }
        if let Some(repo_ids) = query.repository_ids() {
            let quoted = repo_ids
                .iter()
                .map(|r| format!("'{}'", r.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            extra.push(format!("c.repository_id IN ({})", quoted));
        }
        if !extra.is_empty() {
            sql.push_str(&format!(" AND ({})", extra.join(" AND ")));
        }
        sql.push_str(" ORDER BY score DESC LIMIT ?");

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare text search: {}", e)))?;
        let mut rows = stmt
            .query(params![limit as i64])
            .map_err(|e| DomainError::storage(format!("Failed to run text search: {}", e)))?;

        let mut results = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read text row: {}", e)))?
        {
            let score: f32 = row
                .get(10)
                .map_err(|e| DomainError::storage(format!("Failed to read text score: {}", e)))?;
            if score == 0.0 {
                continue;
            }
            let chunk = Self::row_to_chunk(row)
                .map_err(|e| DomainError::storage(format!("Failed to parse text chunk row: {}", e)))?;
            results.push(SearchResult::new(chunk, score));
        }
        Ok(results)
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
            &format!(
                "DELETE FROM \"{}\".embeddings WHERE chunk_id = ?",
                self.namespace
            ),
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
            &format!(
                "DELETE FROM \"{}\".chunks WHERE repository_id = ?",
                self.namespace
            ),
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete chunks: {}", e)))?;

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;
        Ok(())
    }

    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError> {
        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        tx.execute(
            &format!(
                "DELETE FROM \"{0}\".embeddings WHERE chunk_id IN (SELECT id FROM \"{0}\".chunks WHERE repository_id = ? AND file_path = ?)",
                self.namespace
            ),
            params![repository_id, file_path],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete embeddings: {}", e)))?;

        let deleted_count = tx
            .execute(
                &format!(
                    "DELETE FROM \"{}\".chunks WHERE repository_id = ? AND file_path = ?",
                    self.namespace
                ),
                params![repository_id, file_path],
            )
            .map_err(|e| DomainError::storage(format!("Failed to delete chunks: {}", e)))?;

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!(
            "Deleted {} chunks for file {} in repository {}",
            deleted_count, file_path, repository_id
        );
        Ok(deleted_count as u64)
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

        // When text search is enabled, fetch more candidates from each leg so
        // RRF has a meaningful pool to rank. The final result is capped at query.limit().
        let fetch_limit = if query.is_text_search() {
            (query.limit() * 2).max(20)
        } else {
            query.limit()
        };

        let conn = self.conn.lock().await;

        let semantic = Self::run_semantic(&conn, &self.namespace, &array_lit, query, fetch_limit)?;

        if !query.is_text_search() {
            return Ok(semantic);
        }

        let terms: Vec<&str> = query.query().split_whitespace().collect();
        let text = Self::run_text(&conn, &self.namespace, &terms, query, fetch_limit)?;

        debug!(
            "Text search: {} semantic + {} text candidates → fusing",
            semantic.len(),
            text.len()
        );

        Ok(rrf_fuse(semantic, text, query.limit()))
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
