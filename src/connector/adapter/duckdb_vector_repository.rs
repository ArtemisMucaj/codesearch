use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, AccessMode, Config, Connection, Row};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::application::{rrf_fuse, VectorRepository};
use crate::domain::{CodeChunk, DomainError, Embedding, SearchQuery, SearchResult};

/// Maximum number of BM25 candidates fetched per search leg.
/// BM25 is exact keyword matching so a small pool is sufficient;
/// the semantic leg handles broader recall.
const BM25_FETCH_LIMIT: usize = 10;

/// Embedding configuration that must remain consistent across all operations on
/// a given namespace. Stored in the `namespace_config` table and validated on
/// every open; mismatches are hard errors with actionable messages.
#[derive(Debug, Clone)]
pub struct NamespaceEmbeddingConfig {
    /// `"onnx"` or `"api"`.
    pub embedding_target: String,
    /// Model identifier that produced the embeddings (HuggingFace ID or API
    /// model name).  Must stay the same for the lifetime of the namespace to
    /// preserve a consistent embedding space.
    pub embedding_model: String,
    /// Dimensionality of the embedding vectors.  Fixed by the schema of the
    /// `embeddings` table and cannot change after the namespace is first created.
    pub dimensions: usize,
}

pub struct DuckdbVectorRepository {
    conn: Arc<Mutex<Connection>>,
    namespace: String,
    /// Dimensionality of the embedding vectors for this namespace.
    dimensions: usize,
    /// Set to `true` whenever chunk data changes (inserts or deletes).
    /// The FTS index is rebuilt lazily before the next BM25 search.
    fts_dirty: AtomicBool,
    /// `true` when the connection was opened in read-only mode.
    /// In this mode DDL (including `PRAGMA create_fts_index`) is forbidden,
    /// so we never attempt a rebuild and degrade silently when the index is absent.
    read_only: bool,
}

impl DuckdbVectorRepository {
    /// Open (or create) a writable vector repository for `namespace` with the
    /// given embedding configuration.
    ///
    /// **First open** (new namespace): creates the DuckDB schema with
    /// `FLOAT[dimensions]` columns and persists the config in `namespace_config`.
    ///
    /// **Subsequent opens**: loads the stored config and validates it against
    /// the provided one.  A dimension mismatch is a hard error (schema
    /// incompatibility); a model mismatch is also a hard error (different
    /// embedding space — re-index with `codesearch index --force` to fix).
    pub fn new_with_namespace(
        path: &Path,
        namespace: &str,
        cfg: &NamespaceEmbeddingConfig,
    ) -> Result<Self, DomainError> {
        let conn = Connection::open(path)
            .map_err(|e| DomainError::storage(format!("Failed to open DuckDB database: {}", e)))?;

        let schema = namespace.trim();
        let schema_name = if schema.is_empty() { "main" } else { schema };

        let dimensions = Self::initialize(&conn, schema_name, cfg, false)?;

        let fts_already_exists = Self::fts_index_exists(&conn, schema_name);

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: schema_name.to_string(),
            dimensions,
            fts_dirty: AtomicBool::new(!fts_already_exists),
            read_only: false,
        })
    }

    #[allow(dead_code)]
    pub fn in_memory() -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            DomainError::storage(format!("Failed to open DuckDB in-memory DB: {}", e))
        })?;
        let namespace = "main";
        let cfg = NamespaceEmbeddingConfig {
            embedding_target: "onnx".to_string(),
            embedding_model: "mock".to_string(),
            dimensions: 384,
        };
        let dimensions = Self::initialize(&conn, namespace, &cfg, false)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: namespace.to_string(),
            dimensions,
            fts_dirty: AtomicBool::new(true),
            read_only: false,
        })
    }

    /// Opens the database in read-only mode.
    ///
    /// Read-only connections do not acquire the exclusive write lock, so multiple
    /// processes can search the same database file concurrently without conflicts.
    ///
    /// The stored `namespace_config` is read to determine the correct vector
    /// dimensions; the provided `cfg` is validated against it (hard error on
    /// mismatch). Schema initialisation is skipped.
    pub fn new_read_only_with_namespace(
        path: &Path,
        namespace: &str,
        cfg: &NamespaceEmbeddingConfig,
    ) -> Result<Self, DomainError> {
        let config = Config::default()
            .access_mode(AccessMode::ReadOnly)
            .map_err(|e| {
                DomainError::storage(format!("Failed to configure read-only access: {}", e))
            })?;

        let conn = Connection::open_with_flags(path, config).map_err(|e| {
            DomainError::storage(format!("Failed to open DuckDB (read-only): {}", e))
        })?;

        // Load VSS and FTS for query support; INSTALL is DDL and forbidden.
        conn.execute_batch("LOAD vss; SET hnsw_enable_experimental_persistence = true; LOAD fts;")
            .map_err(|e| DomainError::storage(format!("Failed to load extensions: {}", e)))?;

        let schema = namespace.trim();
        let schema_name = if schema.is_empty() { "main" } else { schema };

        // Read stored dimensions (read-only: don't save anything)
        let dimensions = Self::initialize(&conn, schema_name, cfg, true)?;

        let fts_already_exists = Self::fts_index_exists(&conn, schema_name);
        if !fts_already_exists {
            warn!(
                "BM25 index not found for namespace '{}'; keyword search is disabled. \
                 Run 'codesearch index' to build it.",
                schema_name
            );
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: schema_name.to_string(),
            dimensions,
            fts_dirty: AtomicBool::new(!fts_already_exists),
            read_only: true,
        })
    }

    /// Returns a clone of the shared connection Arc.
    /// This allows other adapters to share the same DuckDB connection,
    /// which is necessary because DuckDB only allows one write connection per file.
    pub fn shared_connection(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    /// Returns the vector dimensionality configured for this namespace.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Initialise extensions, global tables, namespace schema, and the
    /// `namespace_config` entry.
    ///
    /// Returns the **effective** dimensions for this namespace:
    /// - If a `namespace_config` row already exists, the stored dimensions are
    ///   used and the provided `cfg` is validated against them (hard errors on
    ///   mismatch).
    /// - If no row exists (new namespace) and `read_only` is `false`, the
    ///   namespace schema is created with `cfg.dimensions` and the config is
    ///   persisted.
    /// - In `read_only` mode the stored dimensions are returned; if the
    ///   namespace has no stored config (DB may not have been indexed yet),
    ///   `cfg.dimensions` is returned as a best-effort fallback.
    fn initialize(
        conn: &Connection,
        schema_name: &str,
        cfg: &NamespaceEmbeddingConfig,
        read_only: bool,
    ) -> Result<usize, DomainError> {
        debug!("Initializing DuckDB with schema: {}", schema_name);

        if read_only {
            // Only load extensions; DDL forbidden.
            // (extensions were already loaded by the caller for read-only path)
            // Read and validate stored namespace config.
            return Self::read_and_validate_namespace_config(conn, schema_name, cfg, read_only);
        }

        // Install and load VSS + FTS.
        conn.execute_batch(
            "INSTALL vss; LOAD vss; SET hnsw_enable_experimental_persistence = true; \
             INSTALL fts; LOAD fts;",
        )
        .map_err(|e| DomainError::storage(format!("Failed to initialize extensions: {}", e)))?;

        // Global tables (not namespace-scoped).
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
                namespace TEXT,
                languages TEXT
            );
            CREATE TABLE IF NOT EXISTS namespace_config (
                namespace TEXT PRIMARY KEY,
                embedding_target TEXT NOT NULL,
                embedding_model TEXT NOT NULL,
                dimensions INTEGER NOT NULL
            );
            "#,
        )
        .map_err(|e| DomainError::storage(format!("Failed to create global tables: {}", e)))?;

        // Read or save namespace_config, obtain effective dimensions.
        let dims = Self::read_and_validate_namespace_config(conn, schema_name, cfg, read_only)?;

        // Namespace schema tables — use effective dims so the FLOAT column is
        // always correct for both new and existing namespaces.
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
                vector FLOAT[{dims}] NOT NULL,
                model TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS embedding_hnsw_idx
                ON "{}".embeddings USING HNSW (vector) WITH (metric = 'cosine');
            "#,
            schema_name,
            schema_name,
            schema_name,
            schema_name,
            dims = dims
        );

        conn.execute_batch(&schema_sql).map_err(|e| {
            DomainError::storage(format!("Failed to initialize namespace schema: {}", e))
        })?;

        debug!("DuckDB schema initialised (namespace={schema_name}, dims={dims})");
        Ok(dims)
    }

    /// Read the stored `namespace_config` row, validate it against `cfg`, and
    /// return the effective dimensions.
    ///
    /// - Existing row: validates `dimensions` and `embedding_model`.  Hard error
    ///   on mismatch so corrupt searches never silently happen.
    /// - No row + write mode: saves `cfg` and returns `cfg.dimensions`.
    /// - No row + read-only mode: returns `cfg.dimensions` as a best-effort
    ///   fallback (the namespace may simply not have been indexed yet).
    fn read_and_validate_namespace_config(
        conn: &Connection,
        namespace: &str,
        cfg: &NamespaceEmbeddingConfig,
        read_only: bool,
    ) -> Result<usize, DomainError> {
        // Attempt to read the stored config; the table may not exist yet in
        // read-only mode (first ever open before any indexing).
        let stored = conn
            .query_row(
                "SELECT embedding_target, embedding_model, dimensions \
                 FROM namespace_config WHERE namespace = ?",
                params![namespace],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as usize,
                    ))
                },
            )
            .ok()
            .map(|(target, model, dims)| (target, model, dims));

        match stored {
            Some((stored_target, stored_model, stored_dims)) => {
                // Hard error: dimension mismatch means the schema cannot be used.
                if stored_dims != cfg.dimensions {
                    return Err(DomainError::invalid_input(format!(
                        "Namespace '{}' was indexed with {}-dimensional embeddings \
                         (model '{}', target '{}') but you are now using {}-dimensional \
                         embeddings (model '{}', target '{}'). \
                         Re-index with `codesearch index --force` using the original \
                         model, or create a new namespace with `--namespace <name>`.",
                        namespace,
                        stored_dims,
                        stored_model,
                        stored_target,
                        cfg.dimensions,
                        cfg.embedding_model,
                        cfg.embedding_target,
                    )));
                }
                // Hard error: model mismatch means vectors live in different spaces.
                if stored_model != cfg.embedding_model {
                    return Err(DomainError::invalid_input(format!(
                        "Namespace '{}' was indexed with embedding model '{}' (target '{}') \
                         but you are now using model '{}' (target '{}'). \
                         Mixed embedding spaces produce meaningless search results. \
                         Re-index with `codesearch index --force` using model '{}', \
                         or create a new namespace with `--namespace <name>`.",
                        namespace,
                        stored_model,
                        stored_target,
                        cfg.embedding_model,
                        cfg.embedding_target,
                        cfg.embedding_model,
                    )));
                }
                debug!(
                    "Namespace '{}' config validated (model='{}', dims={})",
                    namespace, stored_model, stored_dims
                );
                Ok(stored_dims)
            }
            None => {
                if !read_only {
                    // New namespace: persist the config.
                    conn.execute(
                        "INSERT INTO namespace_config \
                         (namespace, embedding_target, embedding_model, dimensions) \
                         VALUES (?, ?, ?, ?)",
                        params![
                            namespace,
                            cfg.embedding_target,
                            cfg.embedding_model,
                            cfg.dimensions as i64
                        ],
                    )
                    .map_err(|e| {
                        DomainError::storage(format!("Failed to save namespace config: {e}"))
                    })?;
                    debug!(
                        "Namespace '{}' config saved (model='{}', dims={})",
                        namespace, cfg.embedding_model, cfg.dimensions
                    );
                }
                Ok(cfg.dimensions)
            }
        }
    }

    // ── FTS helpers ──────────────────────────────────────────────────────────

    /// Returns the DuckDB schema name that the FTS extension creates for our
    /// chunks table, e.g. `fts_main_chunks` for namespace `main`.
    fn fts_schema_name(namespace: &str) -> String {
        format!("fts_{}_chunks", namespace)
    }

    /// Returns `true` if the FTS index for this namespace already exists in
    /// the database (queried via `information_schema.schemata`).
    fn fts_index_exists(conn: &Connection, namespace: &str) -> bool {
        let fts_schema = Self::fts_schema_name(namespace);
        match conn.query_row(
            "SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = ?",
            params![fts_schema],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(count) => count > 0,
            Err(e) => {
                debug!(
                    "Failed to query FTS index existence for namespace '{}' (schema '{}'): {}",
                    namespace, fts_schema, e
                );
                false
            }
        }
    }

    /// Rebuilds the FTS index from scratch for the given namespace.
    ///
    /// Uses `stemmer='none'` so that code identifiers are not stemmed — exact
    /// token matching is more appropriate for source code than natural-language
    /// stemming. The `overwrite=1` flag drops any existing index and recreates it.
    fn rebuild_fts_index(conn: &Connection, namespace: &str) -> Result<(), DomainError> {
        let sql = format!(
            "PRAGMA create_fts_index('{ns}.chunks', 'id', 'content', 'symbol_name', \
             stemmer='none', overwrite=1);",
            ns = namespace
        );
        conn.execute_batch(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to rebuild FTS index: {e}")))
    }

    // ── Private query helpers (synchronous, take &Connection) ────────────────

    fn vector_to_array_literal(&self, vector: &[f32]) -> Result<String, DomainError> {
        if vector.len() != self.dimensions {
            return Err(DomainError::invalid_input(format!(
                "Expected embedding dimension {}, got {}",
                self.dimensions,
                vector.len()
            )));
        }
        let mut s = String::with_capacity(vector.len() * 8);
        s.push('[');
        for (i, v) in vector.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("{}", v));
        }
        s.push(']');
        s.push_str(&format!("::FLOAT[{}]", self.dimensions));
        Ok(s)
    }

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
        sql.push_str(" ORDER BY score DESC LIMIT ?");

        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare semantic search: {}", e))
        })?;
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
            // In hybrid mode the full candidate pool feeds rrf_fuse; apply
            // min_score after fusion instead of dropping candidates here.
            if !query.is_text_search() {
                if let Some(min) = query.min_score() {
                    if score < min {
                        continue;
                    }
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

    /// Performs real Okapi BM25 full-text search using DuckDB's FTS extension.
    ///
    /// The `match_bm25` macro function is called as
    /// `fts_<namespace>_chunks.match_bm25(id, query_string)` and returns a BM25
    /// relevance score for each matching document. Documents that do not match
    /// any query token receive a NULL score and are excluded.
    ///
    /// `stemmer='none'` is used at index time so code identifiers are matched
    /// exactly rather than reduced to their English stem root.
    fn run_text(
        conn: &Connection,
        namespace: &str,
        query: &SearchQuery,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let query_str = query.query().trim().to_string();
        if query_str.is_empty() {
            return Ok(vec![]);
        }

        let fts = Self::fts_schema_name(namespace);

        // The subquery computes the BM25 score for every chunk; the outer query
        // filters out non-matching rows (NULL score) and applies optional column
        // filters before sorting and limiting.
        let mut sql = format!(
            "SELECT sq.id, sq.file_path, sq.content, sq.start_line, sq.end_line, \
             sq.language, sq.node_type, sq.symbol_name, sq.parent_symbol, sq.repository_id, \
             CAST(sq.score AS FLOAT) AS score \
             FROM ( \
                 SELECT c.id, c.file_path, c.content, c.start_line, c.end_line, \
                        c.language, c.node_type, c.symbol_name, c.parent_symbol, c.repository_id, \
                        {fts}.match_bm25(c.id, ?) AS score \
                 FROM \"{ns}\".chunks c \
             ) sq \
             WHERE sq.score IS NOT NULL",
            fts = fts,
            ns = namespace,
        );

        let mut extra: Vec<String> = Vec::new();
        if let Some(languages) = query.languages() {
            let quoted = languages
                .iter()
                .map(|l| format!("'{}'", l.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            extra.push(format!("sq.language IN ({})", quoted));
        }
        if let Some(node_types) = query.node_types() {
            let quoted = node_types
                .iter()
                .map(|t| format!("'{}'", t.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            extra.push(format!("sq.node_type IN ({})", quoted));
        }
        if let Some(repo_ids) = query.repository_ids() {
            let quoted = repo_ids
                .iter()
                .map(|r| format!("'{}'", r.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            extra.push(format!("sq.repository_id IN ({})", quoted));
        }
        if !extra.is_empty() {
            sql.push_str(&format!(" AND ({})", extra.join(" AND ")));
        }
        sql.push_str(" ORDER BY sq.score DESC LIMIT ?");

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare BM25 search: {e}")))?;
        let mut rows = stmt
            .query(params![query_str, limit as i64])
            .map_err(|e| DomainError::storage(format!("Failed to run BM25 search: {e}")))?;

        let mut results = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read BM25 row: {e}")))?
        {
            let score: f32 = row
                .get(10)
                .map_err(|e| DomainError::storage(format!("Failed to read BM25 score: {e}")))?;
            let chunk = Self::row_to_chunk(row).map_err(|e| {
                DomainError::storage(format!("Failed to parse BM25 chunk row: {e}"))
            })?;
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
            let array_lit = self.vector_to_array_literal(embedding.vector())?;
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

        // Mark the FTS index as stale; it will be rebuilt lazily on the next BM25 search.
        self.fts_dirty.store(true, Ordering::Release);

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
        self.fts_dirty.store(true, Ordering::Release);
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
        self.fts_dirty.store(true, Ordering::Release);
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
        self.fts_dirty.store(true, Ordering::Release);

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
        if query_embedding.len() != self.dimensions {
            return Err(DomainError::invalid_input(format!(
                "Expected query embedding dimension {}, got {}",
                self.dimensions,
                query_embedding.len()
            )));
        }

        let array_lit = self.vector_to_array_literal(query_embedding)?;

        let conn = self.conn.lock().await;

        let semantic =
            Self::run_semantic(&conn, &self.namespace, &array_lit, query, query.limit())?;

        if !query.is_text_search() {
            return Ok(semantic);
        }

        // Rebuild the FTS index if the chunk data has changed since last search.
        // This is a lazy rebuild strategy: we pay the rebuild cost once per "dirty"
        // window (i.e., after any sequence of inserts/deletes) rather than after
        // every individual write.
        if self.fts_dirty.load(Ordering::Acquire) {
            if self.read_only {
                // DDL is forbidden in read-only mode; the FTS index was not built
                // during a prior write session. Degrade silently to semantic-only.
                debug!(
                    "FTS index unavailable in read-only mode for namespace '{}'; \
                     run 'codesearch index' to build it. Falling back to semantic-only.",
                    self.namespace
                );
                return Ok(semantic);
            }
            match Self::rebuild_fts_index(&conn, &self.namespace) {
                Ok(()) => {
                    self.fts_dirty.store(false, Ordering::Release);
                    debug!("FTS index rebuilt for namespace '{}'", self.namespace);
                }
                Err(e) => {
                    // FTS extension unavailable or another unexpected failure.
                    warn!(
                        "Failed to rebuild FTS index (falling back to semantic-only): {}",
                        e
                    );
                    return Ok(semantic);
                }
            }
        }

        let text = match Self::run_text(&conn, &self.namespace, query, BM25_FETCH_LIMIT) {
            Ok(results) => results,
            Err(e) => {
                // If BM25 query fails (e.g. FTS schema missing in read-only DB),
                // degrade gracefully to semantic-only results.
                warn!(
                    "BM25 text search failed (falling back to semantic-only): {}",
                    e
                );
                return Ok(semantic);
            }
        };

        let semantic_len = semantic.len();
        let text_len = text.len();
        let mut fused = rrf_fuse(vec![semantic, text], query.limit());
        info!(
            "Hybrid search: {} semantic + {} BM25 candidates → {} after fusion",
            semantic_len,
            text_len,
            fused.len()
        );
        if let Some(min) = query.min_score() {
            fused.retain(|r| r.score() >= min);
        }
        Ok(fused)
    }

    async fn flush(&self) -> Result<(), DomainError> {
        if self.read_only || !self.fts_dirty.load(Ordering::Acquire) {
            return Ok(());
        }
        let conn = self.conn.lock().await;
        Self::rebuild_fts_index(&conn, &self.namespace)?;
        self.fts_dirty.store(false, Ordering::Release);
        info!("BM25 index built for namespace '{}'", self.namespace);
        Ok(())
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

    async fn find_chunks_by_file(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<Vec<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;

        let (sql, use_repo_filter) = if repository_id.is_empty() {
            (
                format!(
                    "SELECT id, file_path, content, start_line, end_line, language, node_type, \
                     symbol_name, parent_symbol, repository_id \
                     FROM \"{}\".chunks WHERE file_path = ? ORDER BY start_line",
                    self.namespace
                ),
                false,
            )
        } else {
            (
                format!(
                    "SELECT id, file_path, content, start_line, end_line, language, node_type, \
                     symbol_name, parent_symbol, repository_id \
                     FROM \"{}\".chunks WHERE file_path = ? AND repository_id = ? \
                     ORDER BY start_line",
                    self.namespace
                ),
                true,
            )
        };

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare file lookup: {e}")))?;

        let mut rows = if use_repo_filter {
            stmt.query(params![file_path, repository_id])
        } else {
            stmt.query(params![file_path])
        }
        .map_err(|e| DomainError::storage(format!("Failed to run file lookup: {e}")))?;

        let mut chunks = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read file lookup row: {e}")))?
        {
            let chunk = Self::row_to_chunk(row).map_err(|e| {
                DomainError::storage(format!("Failed to parse file lookup chunk: {e}"))
            })?;
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    async fn find_chunk_by_symbol(
        &self,
        repository_id: &str,
        symbol: &str,
    ) -> Result<Option<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;

        let (sql, use_repo_filter) = if repository_id.is_empty() {
            (
                format!(
                    "SELECT id, file_path, content, start_line, end_line, language, node_type, \
                     symbol_name, parent_symbol, repository_id \
                     FROM \"{}\".chunks \
                     WHERE symbol_name = ? \
                     ORDER BY (end_line - start_line) ASC \
                     LIMIT 1",
                    self.namespace
                ),
                false,
            )
        } else {
            (
                format!(
                    "SELECT id, file_path, content, start_line, end_line, language, node_type, \
                     symbol_name, parent_symbol, repository_id \
                     FROM \"{}\".chunks \
                     WHERE symbol_name = ? AND repository_id = ? \
                     ORDER BY (end_line - start_line) ASC \
                     LIMIT 1",
                    self.namespace
                ),
                true,
            )
        };

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare symbol lookup: {e}")))?;

        let mut rows = if use_repo_filter {
            stmt.query(params![symbol, repository_id])
        } else {
            stmt.query(params![symbol])
        }
        .map_err(|e| DomainError::storage(format!("Failed to run symbol lookup: {e}")))?;

        let chunk = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read symbol lookup row: {e}")))?
            .map(|row| Self::row_to_chunk(row))
            .transpose()
            .map_err(|e| {
                DomainError::storage(format!("Failed to parse symbol lookup chunk: {e}"))
            })?;

        Ok(chunk)
    }
}
