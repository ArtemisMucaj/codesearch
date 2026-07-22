use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, params_from_iter, AccessMode, Config, Connection, Row};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::application::{rrf_fuse, VectorRepository};
use crate::domain::{CodeChunk, DomainError, Embedding, SearchQuery, SearchResult};

/// Maximum number of BM25 candidates fetched per search leg.
/// BM25 is exact keyword matching so a small pool is sufficient;
/// the semantic leg handles broader recall.
const BM25_FETCH_LIMIT: usize = 10;

/// Over-fetch multiplier applied to the HNSW candidate pass when the query
/// carries column filters (language / node_type / repository).  The index scan
/// cannot apply those filters itself, so extra nearest neighbours are fetched
/// and filtered afterwards.
const HNSW_FILTER_OVERFETCH_MULTIPLIER: usize = 4;

/// Additive head-room for the filtered HNSW candidate pass, so that small
/// limits (e.g. the default 10) still survive aggressive filters.
const HNSW_FILTER_OVERFETCH_FLOOR: usize = 64;

/// Number of embeddings written per multi-row INSERT statement.  Each row
/// carries its vector as an inline array literal, so one statement replaces
/// what used to be one prepare + execute round-trip per embedding.
const EMBEDDING_INSERT_BATCH: usize = 128;

use super::NO_EMBEDDINGS_MODEL;

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
    /// User-facing namespace name (e.g. `homeframework`). Used as the
    /// `namespace_config` primary key and in logs/errors. Never interpolated
    /// into a SQL identifier.
    namespace: String,
    /// Randomly-generated schema token (e.g. `ns_4f7a2c91`) that backs this
    /// namespace's DuckDB schema. Safe by construction — every character is
    /// generated, so it is always a valid bare identifier regardless of what
    /// the user named the namespace. This is the value interpolated into all
    /// schema-qualified SQL and the FTS PRAGMA. Resolved once and stored in
    /// `namespace_config.schema_token`.
    schema: String,
    /// Dimensionality of the embedding vectors for this namespace.
    dimensions: usize,
    /// Set to `true` whenever chunk data changes (inserts or deletes).
    /// The FTS index is rebuilt lazily before the next BM25 search.
    fts_dirty: AtomicBool,
    /// `true` when the connection was opened in read-only mode.
    /// In this mode DDL (including `PRAGMA create_fts_index`) is forbidden,
    /// so we never attempt a rebuild and degrade silently when the index is absent.
    read_only: bool,
    /// Memoized "store holds at least one embedding" fact.  Once vectors are
    /// observed they are never all removed mid-process by the search path, so
    /// a `true` result is cached and later `has_embeddings` calls skip the
    /// probe query.  `false` is re-probed (an indexing run may add vectors).
    has_vectors: AtomicBool,
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

        let trimmed = namespace.trim();
        let namespace = if trimmed.is_empty() { "main" } else { trimmed };

        let (schema, dimensions) = Self::initialize(&conn, namespace, cfg, false)?;

        let fts_already_exists = Self::fts_index_exists(&conn, &schema);

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: namespace.to_string(),
            schema,
            dimensions,
            fts_dirty: AtomicBool::new(!fts_already_exists),
            read_only: false,
            has_vectors: AtomicBool::new(false),
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
        let (schema, dimensions) = Self::initialize(&conn, namespace, &cfg, false)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: namespace.to_string(),
            schema,
            dimensions,
            fts_dirty: AtomicBool::new(true),
            read_only: false,
            has_vectors: AtomicBool::new(false),
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

        let trimmed = namespace.trim();
        let namespace = if trimmed.is_empty() { "main" } else { trimmed };

        // Read stored dimensions and schema token (read-only: don't save anything).
        let (schema, dimensions) = Self::initialize(&conn, namespace, cfg, true)?;

        let fts_already_exists = Self::fts_index_exists(&conn, &schema);
        if !fts_already_exists {
            warn!(
                "BM25 index not found for namespace '{}'; keyword search is disabled. \
                 Run 'codesearch index' to build it.",
                namespace
            );
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace: namespace.to_string(),
            schema,
            dimensions,
            fts_dirty: AtomicBool::new(!fts_already_exists),
            read_only: true,
            has_vectors: AtomicBool::new(false),
        })
    }

    /// Returns a clone of the shared connection Arc.
    /// This allows other adapters to share the same DuckDB connection,
    /// which is necessary because DuckDB only allows one write connection per file.
    pub fn shared_connection(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    /// A read-view of a SIBLING namespace over the same shared connection, for
    /// cross-namespace chunk lookups (e.g. explain snippets when the server
    /// booted on a different namespace than the repository being explained —
    /// opening a second connection would hit the file lock this one holds).
    /// Resolves the sibling's schema token directly and skips embedding-config
    /// validation: the view serves text queries (chunks), never vectors, so a
    /// different embedding model over there is irrelevant.
    pub async fn namespace_view(&self, namespace: &str) -> Result<Self, DomainError> {
        let (schema, dimensions) = {
            let conn = self.conn.lock().await;
            conn.query_row(
                "SELECT schema_token, dimensions FROM namespace_config WHERE namespace = ?",
                params![namespace],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize)),
            )
            .map_err(|e| {
                DomainError::storage(format!(
                    "Namespace '{namespace}' has no stored config (never indexed?): {e}"
                ))
            })?
        };
        Ok(Self {
            conn: Arc::clone(&self.conn),
            namespace: namespace.to_string(),
            schema,
            dimensions,
            // A view never (re)builds indexes — it is read-only by design.
            fts_dirty: AtomicBool::new(false),
            read_only: true,
            has_vectors: AtomicBool::new(false),
        })
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
        namespace: &str,
        cfg: &NamespaceEmbeddingConfig,
        read_only: bool,
    ) -> Result<(String, usize), DomainError> {
        debug!("Initializing DuckDB for namespace: {}", namespace);

        if read_only {
            // Only load extensions; DDL forbidden.
            // (extensions were already loaded by the caller for read-only path)
            // Read and validate stored namespace config; resolve the schema token.
            return Self::read_and_validate_namespace_config(conn, namespace, cfg, read_only);
        }

        Self::init_extensions_and_global_tables(conn)?;

        // Read or save namespace_config, obtaining the schema token and effective
        // dimensions. The token is generated on first creation and read back
        // thereafter; see `read_and_validate_namespace_config`.
        let (schema, dims) =
            Self::read_and_validate_namespace_config(conn, namespace, cfg, read_only)?;

        // Namespace schema tables — use effective dims so the FLOAT column is
        // always correct for both new and existing namespaces. The schema
        // identifier is the generated token, so it is always a safe bare name.
        let schema_sql = format!(
            r#"
            CREATE SCHEMA IF NOT EXISTS "{schema}";
            CREATE TABLE IF NOT EXISTS "{schema}".chunks (
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
            CREATE TABLE IF NOT EXISTS "{schema}".embeddings (
                chunk_id TEXT PRIMARY KEY,
                vector FLOAT[{dims}] NOT NULL,
                model TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS embedding_hnsw_idx
                ON "{schema}".embeddings USING HNSW (vector) WITH (metric = 'cosine');
            "#,
            schema = schema,
            dims = dims
        );

        conn.execute_batch(&schema_sql).map_err(|e| {
            DomainError::storage(format!("Failed to initialize namespace schema: {}", e))
        })?;

        debug!("DuckDB schema initialised (namespace={namespace}, schema={schema}, dims={dims})");
        Ok((schema, dims))
    }

    /// Generates a fresh, collision-resistant schema token for a new namespace.
    ///
    /// The token is `ns_` followed by a random 32-hex-character UUIDv4 (dashes
    /// stripped). Because every character after the prefix is generated, the
    /// result is always a valid bare SQL identifier — independent of whatever
    /// characters the user put in the namespace name.
    fn generate_schema_token() -> String {
        format!("ns_{}", uuid::Uuid::new_v4().simple())
    }

    /// Install/load the VSS + FTS extensions and create the global
    /// (non-namespace-scoped) tables.  Idempotent.
    fn init_extensions_and_global_tables(conn: &Connection) -> Result<(), DomainError> {
        conn.execute_batch(
            "INSTALL vss; LOAD vss; SET hnsw_enable_experimental_persistence = true; \
             INSTALL fts; LOAD fts;",
        )
        .map_err(|e| DomainError::storage(format!("Failed to initialize extensions: {}", e)))?;

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
                git_remote TEXT,
                languages TEXT
            );
            CREATE TABLE IF NOT EXISTS namespace_config (
                namespace TEXT PRIMARY KEY,
                schema_token TEXT NOT NULL,
                embedding_target TEXT NOT NULL,
                embedding_model TEXT NOT NULL,
                dimensions INTEGER NOT NULL
            );
            "#,
        )
        .map_err(|e| DomainError::storage(format!("Failed to create global tables: {}", e)))?;

        Self::guard_against_legacy_namespace_config(conn)
    }

    /// Reject databases whose `namespace_config` predates the `schema_token`
    /// column with a clear, actionable error instead of a cryptic SQL failure.
    ///
    /// `CREATE TABLE IF NOT EXISTS` leaves an older table untouched, so a DB
    /// indexed by a prior release keeps a `namespace_config` with no
    /// `schema_token` column; every schema-qualified query would then fail deep
    /// in the search path. Schema tokens are new in this version and unreleased,
    /// so there is no data worth migrating — we tell the user to re-index rather
    /// than carry migration machinery.
    fn guard_against_legacy_namespace_config(conn: &Connection) -> Result<(), DomainError> {
        // The table always exists here (just created above if absent). A missing
        // `schema_token` column means the table is the pre-token legacy shape.
        let has_schema_token: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_name = 'namespace_config' AND column_name = 'schema_token'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .unwrap_or(false);

        if has_schema_token {
            return Ok(());
        }

        Err(DomainError::invalid_input(
            "This database was created by an older codesearch version and is no \
             longer compatible. Delete '~/.codesearch/codesearch.duckdb' and \
             re-index with `codesearch index`.",
        ))
    }

    /// Create `namespace` with a fixed embedding configuration, failing when
    /// it already exists — a namespace's embedding setup is decided once, at
    /// creation, and inherited by every later index/search run against it.
    ///
    /// Only writes configuration and empty schema; no embedding model is
    /// loaded or downloaded.
    pub fn create_namespace(
        path: &Path,
        namespace: &str,
        cfg: &NamespaceEmbeddingConfig,
    ) -> Result<(), DomainError> {
        let conn = Connection::open(path)
            .map_err(|e| DomainError::storage(format!("Failed to open DuckDB database: {}", e)))?;

        let trimmed = namespace.trim();
        let namespace = if trimmed.is_empty() { "main" } else { trimmed };

        // Global tables must exist before the existence probe on a fresh database.
        Self::init_extensions_and_global_tables(&conn)?;

        let existing_model: Option<String> = conn
            .query_row(
                "SELECT embedding_model FROM namespace_config WHERE namespace = ?",
                params![namespace],
                |row| row.get(0),
            )
            .ok();
        if let Some(model) = existing_model {
            return Err(DomainError::invalid_input(format!(
                "Namespace '{}' already exists (embedding model '{}'). \
                 A namespace's embedding configuration is fixed at creation — \
                 choose a different name.",
                namespace, model
            )));
        }

        Self::initialize(&conn, namespace, cfg, false)?;
        Ok(())
    }

    /// Read the stored `namespace_config` row, validate it against `cfg`, and
    /// return the namespace's schema token and effective dimensions.
    ///
    /// - Existing row: validates `dimensions` and `embedding_model` (hard error
    ///   on mismatch so corrupt searches never silently happen) and returns the
    ///   stored `schema_token`.
    /// - No row + write mode: generates a fresh schema token, saves `cfg`, and
    ///   returns `(token, cfg.dimensions)`.
    /// - No row + read-only mode: returns a freshly generated token and
    ///   `cfg.dimensions` as a best-effort fallback. The namespace simply has
    ///   no schema yet, so schema-qualified queries find nothing and the search
    ///   path degrades gracefully.
    fn read_and_validate_namespace_config(
        conn: &Connection,
        namespace: &str,
        cfg: &NamespaceEmbeddingConfig,
        read_only: bool,
    ) -> Result<(String, usize), DomainError> {
        // Attempt to read the stored config; the table may not exist yet in
        // read-only mode (first ever open before any indexing).
        let stored = conn
            .query_row(
                "SELECT schema_token, embedding_target, embedding_model, dimensions \
                 FROM namespace_config WHERE namespace = ?",
                params![namespace],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)? as usize,
                    ))
                },
            )
            .ok();

        match stored {
            Some((stored_token, stored_target, stored_model, stored_dims)) => {
                // A "none" model on either side means --no-embeddings mode:
                // there is no embedding space to protect, so the mismatch
                // checks below don't apply.  Warn on mixed usage — chunks
                // indexed while embeddings were disabled simply have no
                // vectors and only surface through the keyword and graph legs.
                if stored_model == NO_EMBEDDINGS_MODEL || cfg.embedding_model == NO_EMBEDDINGS_MODEL
                {
                    if stored_model != cfg.embedding_model {
                        warn!(
                            "Namespace '{}' mixes no-embeddings and embedding modes \
                             (stored model '{}', current '{}'). Chunks indexed without \
                             embeddings are only found by keyword/graph search; \
                             re-index with `codesearch index --force` for a uniform store.",
                            namespace, stored_model, cfg.embedding_model
                        );
                    }
                    return Ok((stored_token, stored_dims));
                }
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
                    "Namespace '{}' config validated (schema='{}', model='{}', dims={})",
                    namespace, stored_token, stored_model, stored_dims
                );
                Ok((stored_token, stored_dims))
            }
            None => {
                // Generate a fresh, safe-by-construction schema token for this
                // namespace. In read-only mode we still return one so callers
                // have a valid (if empty) schema to address; it is never
                // persisted because DDL is forbidden. A legacy pre-token DB never
                // reaches here in write mode — `guard_against_legacy_namespace_config`
                // rejects it up front with an actionable re-index error.
                let schema_token = Self::generate_schema_token();
                if !read_only {
                    // New namespace: persist the config together with its token.
                    conn.execute(
                        "INSERT INTO namespace_config \
                         (namespace, schema_token, embedding_target, embedding_model, dimensions) \
                         VALUES (?, ?, ?, ?, ?)",
                        params![
                            namespace,
                            schema_token,
                            cfg.embedding_target,
                            cfg.embedding_model,
                            cfg.dimensions as i64
                        ],
                    )
                    .map_err(|e| {
                        DomainError::storage(format!("Failed to save namespace config: {e}"))
                    })?;
                    debug!(
                        "Namespace '{}' config saved (schema='{}', model='{}', dims={})",
                        namespace, schema_token, cfg.embedding_model, cfg.dimensions
                    );
                }
                Ok((schema_token, cfg.dimensions))
            }
        }
    }

    // ── FTS helpers ──────────────────────────────────────────────────────────

    /// Returns the DuckDB schema name that the FTS extension creates for a
    /// namespace's chunks table.  The extension derives this as `fts_<table>`,
    /// where `<table>` is the fully-qualified `create_fts_index` argument with
    /// its dot replaced by `_`.  We index `"<schema>".chunks`, so the derived
    /// schema is `fts_<schema>_chunks`.  Since `schema` is a generated token
    /// (`ns_<hex>`), the result is always a valid bare identifier.
    fn fts_schema_name(schema: &str) -> String {
        format!("fts_{schema}_chunks")
    }

    /// Returns `true` if the FTS index for this schema already exists in the
    /// database (queried via `information_schema.schemata`).
    fn fts_index_exists(conn: &Connection, schema: &str) -> bool {
        let fts_schema = Self::fts_schema_name(schema);
        match conn.query_row(
            "SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = ?",
            params![fts_schema],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(count) => count > 0,
            Err(e) => {
                debug!(
                    "Failed to query FTS index existence for schema '{}': {}",
                    fts_schema, e
                );
                false
            }
        }
    }

    /// Rebuilds the FTS index from scratch for the given schema token.
    ///
    /// Indexes the real `"<schema>".chunks` table directly. The schema is a
    /// generated token (`ns_<hex>`), so it is always a bare identifier the
    /// FTS PRAGMA's simplified parser accepts — no sanitizing view is needed.
    ///
    /// Uses `stemmer='none'` so that code identifiers are not stemmed — exact
    /// token matching is more appropriate for source code than natural-language
    /// stemming. The `overwrite=1` flag drops any existing index and recreates it.
    fn rebuild_fts_index(conn: &Connection, schema: &str) -> Result<(), DomainError> {
        let pragma_sql = format!(
            "PRAGMA create_fts_index('{schema}.chunks', 'id', 'content', 'symbol_name', \
             stemmer='none', overwrite=1);",
            schema = schema,
        );
        conn.execute_batch(&pragma_sql)
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

    /// SQL `c.<column> IN (...)` clauses for the optional query filters, with
    /// values single-quote escaped.  Shared by the candidate and full-scan
    /// semantic paths.
    fn filter_clauses(query: &SearchQuery) -> Vec<String> {
        let quote_list = |values: &[String]| {
            values
                .iter()
                .map(|v| format!("'{}'", v.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",")
        };

        let mut clauses = Vec::new();
        if let Some(languages) = query.languages() {
            clauses.push(format!("c.language IN ({})", quote_list(languages)));
        }
        if let Some(node_types) = query.node_types() {
            clauses.push(format!("c.node_type IN ({})", quote_list(node_types)));
        }
        if let Some(repo_ids) = query.repository_ids() {
            clauses.push(format!("c.repository_id IN ({})", quote_list(repo_ids)));
        }
        clauses
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

    /// Two-stage semantic search that keeps the first stage in the exact shape
    /// DuckDB's VSS extension rewrites into an HNSW index scan:
    /// `ORDER BY array_cosine_distance(vector, <const>) LIMIT n` on the bare
    /// embeddings table — no join, no filters, no derived expression in the
    /// ORDER BY.  The second stage joins chunk metadata for the candidate ids
    /// and applies the query filters.
    ///
    /// When filters are present the candidate pass over-fetches; if the
    /// filters still consume too many candidates, the exhaustive scan runs as
    /// a fallback so results are never worse than before.
    fn run_semantic(
        conn: &Connection,
        namespace: &str,
        array_lit: &str,
        query: &SearchQuery,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DomainError> {
        if limit == 0 {
            return Ok(vec![]);
        }

        let has_filters = query.languages().is_some()
            || query.node_types().is_some()
            || query.repository_ids().is_some();
        let fetch = if has_filters {
            limit * HNSW_FILTER_OVERFETCH_MULTIPLIER + HNSW_FILTER_OVERFETCH_FLOOR
        } else {
            limit
        };

        let candidates = Self::run_hnsw_candidates(conn, namespace, array_lit, fetch)?;
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        let exhausted_table = candidates.len() < fetch;
        let results = Self::fetch_candidate_chunks(conn, namespace, &candidates, query, limit)?;

        // Filters ate too much of the over-fetched pool and more rows exist:
        // fall back to the exhaustive scan to preserve recall.
        if has_filters && results.len() < limit && !exhausted_table {
            debug!(
                "HNSW candidate pass returned {}/{} results after filtering; \
                 falling back to full scan",
                results.len(),
                limit
            );
            return Self::run_semantic_full_scan(conn, namespace, array_lit, query, limit);
        }

        Ok(results)
    }

    /// Stage 1: nearest-neighbour candidate ids via the HNSW index.
    ///
    /// `fetch` is inlined as a literal because a parameterised LIMIT prevents
    /// the VSS optimizer from matching the index-scan pattern.
    fn run_hnsw_candidates(
        conn: &Connection,
        namespace: &str,
        array_lit: &str,
        fetch: usize,
    ) -> Result<Vec<(String, f32)>, DomainError> {
        let sql = format!(
            "SELECT chunk_id, array_cosine_distance(vector, {array_lit}) AS dist \
             FROM \"{schema}\".embeddings \
             ORDER BY array_cosine_distance(vector, {array_lit}) \
             LIMIT {fetch}",
            array_lit = array_lit,
            schema = namespace,
            fetch = fetch,
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare HNSW candidate query: {}", e))
        })?;
        let mut rows = stmt
            .query([])
            .map_err(|e| DomainError::storage(format!("Failed to run HNSW candidates: {}", e)))?;

        let mut candidates = Vec::with_capacity(fetch);
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read candidate row: {}", e)))?
        {
            let chunk_id: String = row
                .get(0)
                .map_err(|e| DomainError::storage(format!("Failed to read candidate id: {}", e)))?;
            let dist: f32 = row.get(1).map_err(|e| {
                DomainError::storage(format!("Failed to read candidate distance: {}", e))
            })?;
            candidates.push((chunk_id, dist));
        }
        Ok(candidates)
    }

    /// Stage 2: join chunk metadata for the candidate ids, apply query
    /// filters, and re-attach the similarity scores from stage 1.
    fn fetch_candidate_chunks(
        conn: &Connection,
        namespace: &str,
        candidates: &[(String, f32)],
        query: &SearchQuery,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let id_list = candidates
            .iter()
            .map(|(id, _)| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");

        let mut sql = format!(
            "SELECT \
                c.id, c.file_path, c.content, c.start_line, c.end_line, c.language, c.node_type, \
                c.symbol_name, c.parent_symbol, c.repository_id \
             FROM \"{schema}\".chunks c \
             WHERE c.id IN ({id_list})",
            schema = namespace,
            id_list = id_list,
        );
        for clause in Self::filter_clauses(query) {
            sql.push_str(" AND ");
            sql.push_str(&clause);
        }

        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare candidate chunk lookup: {}", e))
        })?;
        let mut rows = stmt.query([]).map_err(|e| {
            DomainError::storage(format!("Failed to run candidate chunk lookup: {}", e))
        })?;

        let mut chunks_by_id: HashMap<String, CodeChunk> = HashMap::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read chunk row: {}", e)))?
        {
            let chunk = Self::row_to_chunk(row)
                .map_err(|e| DomainError::storage(format!("Failed to parse chunk row: {}", e)))?;
            chunks_by_id.insert(chunk.id().to_string(), chunk);
        }

        // Candidates are already ordered by ascending distance (descending
        // similarity), so a single ordered pass assembles the final list.
        let mut results = Vec::with_capacity(limit);
        for (id, dist) in candidates {
            let Some(chunk) = chunks_by_id.remove(id) else {
                continue; // filtered out or orphaned embedding
            };
            let score = 1.0 - dist;
            if !query.is_text_search() && query.min_score().is_some_and(|min| score < min) {
                continue;
            }
            results.push(SearchResult::new(chunk, score));
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    /// Exhaustive fallback: the original join + sort over every embedding.
    /// Only used when the filtered HNSW candidate pass cannot fill `limit`.
    fn run_semantic_full_scan(
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

        let where_clauses = Self::filter_clauses(query);
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
            if !query.is_text_search() && query.min_score().is_some_and(|min| score < min) {
                continue;
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
                        \"{fts}\".match_bm25(c.id, ?) AS score \
                 FROM \"{ns}\".chunks c \
             ) sq \
             WHERE sq.score IS NOT NULL",
            fts = fts.replace('"', "\"\""),
            ns = namespace.replace('"', "\"\""),
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
        // An empty embeddings slice is a chunks-only save (--no-embeddings
        // mode); any other length mismatch is a caller bug.
        if !embeddings.is_empty() && chunks.len() != embeddings.len() {
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
                        self.schema
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

        // Note: The array literals must be part of the SQL statement (not
        // parameterized) because DuckDB FLOAT[N] doesn't support parameterization.
        // This is safe since the arrays are constructed from our embedding data,
        // not user input.
        for batch in embeddings.chunks(EMBEDDING_INSERT_BATCH) {
            let mut values = String::new();
            let mut bind: Vec<&str> = Vec::with_capacity(batch.len() * 2);
            for (i, embedding) in batch.iter().enumerate() {
                let array_lit = self.vector_to_array_literal(embedding.vector())?;
                if i > 0 {
                    values.push(',');
                }
                values.push_str("(?, ");
                values.push_str(&array_lit);
                values.push_str(", ?)");
                bind.push(embedding.chunk_id());
                bind.push(embedding.model());
            }
            let sql = format!(
                "INSERT OR REPLACE INTO \"{}\".embeddings (chunk_id, vector, model) \
                VALUES {}",
                self.schema, values
            );
            tx.execute(&sql, params_from_iter(bind)).map_err(|e| {
                DomainError::storage(format!(
                    "Failed to insert batch of {} embeddings: {}",
                    batch.len(),
                    e
                ))
            })?;
        }

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        if !embeddings.is_empty() {
            self.has_vectors.store(true, Ordering::Release);
        }
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
                self.schema
            ),
            params![chunk_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete embedding: {}", e)))?;
        tx.execute(
            &format!("DELETE FROM \"{}\".chunks WHERE id = ?", self.schema),
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
                self.schema
            ),
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete embeddings: {}", e)))?;

        tx.execute(
            &format!(
                "DELETE FROM \"{}\".chunks WHERE repository_id = ?",
                self.schema
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
                self.schema
            ),
            params![repository_id, file_path],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete embeddings: {}", e)))?;

        let deleted_count = tx
            .execute(
                &format!(
                    "DELETE FROM \"{}\".chunks WHERE repository_id = ? AND file_path = ?",
                    self.schema
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

    async fn delete_by_file_paths(
        &self,
        repository_id: &str,
        file_paths: &[&str],
    ) -> Result<u64, DomainError> {
        if file_paths.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        let mut del_emb = tx
            .prepare(&format!(
                "DELETE FROM \"{ns}\".embeddings WHERE chunk_id IN \
                 (SELECT id FROM \"{ns}\".chunks \
                  WHERE repository_id = ? AND file_path = ?)",
                ns = self.schema
            ))
            .map_err(|e| {
                DomainError::storage(format!("Failed to prepare batch emb delete: {}", e))
            })?;
        let mut del_chunk = tx
            .prepare(&format!(
                "DELETE FROM \"{ns}\".chunks \
                 WHERE repository_id = ? AND file_path = ?",
                ns = self.schema
            ))
            .map_err(|e| {
                DomainError::storage(format!("Failed to prepare batch chunk delete: {}", e))
            })?;

        let mut total = 0u64;
        for path in file_paths {
            del_emb
                .execute(params![repository_id, path])
                .map_err(|e| DomainError::storage(format!("Failed to delete embeddings: {}", e)))?;
            total += del_chunk
                .execute(params![repository_id, path])
                .map_err(|e| DomainError::storage(format!("Failed to delete chunks: {}", e)))?
                as u64;
        }

        drop(del_emb);
        drop(del_chunk);
        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;
        self.fts_dirty.store(true, Ordering::Release);

        debug!(
            "Batch-deleted {} chunks for {} files in repository {}",
            total,
            file_paths.len(),
            repository_id
        );
        Ok(total)
    }

    async fn search(
        &self,
        query_embedding: Option<&[f32]>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        if let Some(embedding) = query_embedding {
            if embedding.len() != self.dimensions {
                return Err(DomainError::invalid_input(format!(
                    "Expected query embedding dimension {}, got {}",
                    self.dimensions,
                    embedding.len()
                )));
            }
        }

        let conn = self.conn.lock().await;

        // `None` requests a text-only search (no embeddings indexed); the
        // semantic leg is skipped entirely.
        let semantic = match query_embedding {
            None => Vec::new(),
            Some(embedding) => {
                let array_lit = self.vector_to_array_literal(embedding)?;
                Self::run_semantic(&conn, &self.schema, &array_lit, query, query.limit())?
            }
        };

        if !query.is_text_search() && query_embedding.is_some() {
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
            match Self::rebuild_fts_index(&conn, &self.schema) {
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

        // With no semantic candidates the BM25 leg is the only source of
        // results, so it must honour the full requested limit instead of the
        // small hybrid headroom.
        let text_fetch_limit = if semantic.is_empty() {
            query.limit().max(BM25_FETCH_LIMIT)
        } else {
            BM25_FETCH_LIMIT
        };
        let text = match Self::run_text(&conn, &self.schema, query, text_fetch_limit) {
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
        // A failed FTS rebuild must not abort the whole indexing run: the flush
        // shares its connection with the chunk/channel/call-graph writes, so
        // propagating the error here discards data that was already committed and
        // leaves the run non-zero. BM25 is an optional accelerator — degrade to
        // semantic-only (matching the lazy-rebuild path in `search`) and let the
        // index complete. `fts_dirty` stays set so the next search retries.
        match Self::rebuild_fts_index(&conn, &self.schema) {
            Ok(()) => {
                self.fts_dirty.store(false, Ordering::Release);
                info!("BM25 index built for namespace '{}'", self.namespace);
            }
            Err(e) => {
                warn!(
                    "Failed to build BM25 index for namespace '{}' \
                     (keyword search disabled until the next successful build): {}",
                    self.namespace, e
                );
            }
        }
        Ok(())
    }

    async fn has_embeddings(&self) -> Result<bool, DomainError> {
        // Vectors are only ever added mid-process, so a `true` answer is
        // stable and skips the probe on every subsequent search.
        if self.has_vectors.load(Ordering::Acquire) {
            return Ok(true);
        }
        let conn = self.conn.lock().await;
        let exists: bool = conn
            .query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM \"{}\".embeddings)",
                    self.schema
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|e| DomainError::storage(format!("Failed to probe embeddings: {}", e)))?;
        if exists {
            self.has_vectors.store(true, Ordering::Release);
        }
        Ok(exists)
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{}\".chunks", self.schema),
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
                    self.schema
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
                    self.schema
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
        class_hint: Option<&str>,
    ) -> Result<Option<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;

        // When a class hint is available (e.g. "GenericUtils" from "GenericUtils#getIp"),
        // rank chunks whose file_path contains that hint higher so that ambiguous short
        // names (same method name in multiple classes) resolve to the right definition.
        // Fall back to the smallest chunk (tightest scope) as the tiebreaker.
        let file_rank_expr = if class_hint.is_some() {
            "CASE WHEN file_path LIKE ? THEN 0 ELSE 1 END"
        } else {
            "0"
        };

        let (sql, use_repo_filter) = if repository_id.is_empty() {
            (
                format!(
                    "SELECT id, file_path, content, start_line, end_line, language, node_type, \
                     symbol_name, parent_symbol, repository_id \
                     FROM \"{}\".chunks \
                     WHERE symbol_name = ? \
                     ORDER BY {file_rank_expr}, (end_line - start_line) ASC \
                     LIMIT 1",
                    self.schema
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
                     ORDER BY {file_rank_expr}, (end_line - start_line) ASC \
                     LIMIT 1",
                    self.schema
                ),
                true,
            )
        };

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare symbol lookup: {e}")))?;

        // Build the parameter list dynamically based on which optional values are present.
        // Order: symbol, [repository_id], [file_hint_pattern]
        // The file_rank_expr `?` appears in ORDER BY which DuckDB evaluates after WHERE,
        // so the hint parameter comes last.
        let file_hint_pattern = class_hint.map(|h| format!("%{}%", h));

        let mut rows = match (use_repo_filter, file_hint_pattern.as_deref()) {
            (false, None) => stmt.query(params![symbol]),
            (false, Some(hint)) => stmt.query(params![symbol, hint]),
            (true, None) => stmt.query(params![symbol, repository_id]),
            (true, Some(hint)) => stmt.query(params![symbol, repository_id, hint]),
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

    async fn find_chunks_by_symbols(
        &self,
        repository_id: &str,
        symbols: &[&str],
    ) -> Result<Vec<CodeChunk>, DomainError> {
        if symbols.is_empty() {
            return Ok(vec![]);
        }

        // Single scan for every symbol instead of one point query per symbol
        // (chunks.symbol_name is unindexed, so each point query is a scan).
        let symbol_list = symbols
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");

        let mut sql = format!(
            "SELECT id, file_path, content, start_line, end_line, language, node_type, \
             symbol_name, parent_symbol, repository_id \
             FROM \"{}\".chunks WHERE symbol_name IN ({})",
            self.schema, symbol_list
        );
        if !repository_id.is_empty() {
            sql.push_str(" AND repository_id = ?");
        }

        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare batch symbol lookup: {e}"))
        })?;
        let mut rows = if repository_id.is_empty() {
            stmt.query([])
        } else {
            stmt.query(params![repository_id])
        }
        .map_err(|e| DomainError::storage(format!("Failed to run batch symbol lookup: {e}")))?;

        let mut chunks = Vec::new();
        while let Some(row) = rows.next().map_err(|e| {
            DomainError::storage(format!("Failed to read batch symbol lookup row: {e}"))
        })? {
            let chunk = Self::row_to_chunk(row).map_err(|e| {
                DomainError::storage(format!("Failed to parse batch symbol lookup chunk: {e}"))
            })?;
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    async fn get_symbol_to_file_map(
        &self,
        repository_id: &str,
    ) -> Result<Vec<(String, String)>, DomainError> {
        let conn = self.conn.lock().await;

        let sql = format!(
            "SELECT symbol_name, file_path \
             FROM \"{}\".chunks \
             WHERE repository_id = ? AND symbol_name IS NOT NULL",
            self.schema
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare symbol map query: {e}"))
        })?;

        let mut rows = stmt
            .query(params![repository_id])
            .map_err(|e| DomainError::storage(format!("Failed to run symbol map query: {e}")))?;

        let mut result = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| DomainError::storage(format!("Failed to read symbol map row: {e}")))?
        {
            let symbol: String = row
                .get(0)
                .map_err(|e| DomainError::storage(format!("Failed to read symbol_name: {e}")))?;
            let file: String = row
                .get(1)
                .map_err(|e| DomainError::storage(format!("Failed to read file_path: {e}")))?;
            result.push((symbol, file));
        }

        Ok(result)
    }
}
