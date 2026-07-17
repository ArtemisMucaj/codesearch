//! DuckDB-backed [`MemoryRepository`].
//!
//! Memory lives in its own database file (`memory.duckdb` inside the data
//! directory), deliberately separate from the code index (`codesearch.duckdb`)
//! so session imports never contend with indexing and the memory store can be
//! inspected or wiped independently.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection, Row};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::{MemoryRepository, MemoryStats};
use crate::domain::{
    DomainError, DreamRun, ImportedSession, MemoryItem, MemoryKind, MemoryNode, NodeKind,
};

/// File name of the memory database inside the data directory.
pub const MEMORY_DB_FILE: &str = "memory.duckdb";

pub struct DuckdbMemoryRepository {
    conn: Arc<Mutex<Connection>>,
    dimensions: usize,
}

impl DuckdbMemoryRepository {
    /// Open (or create) the memory database at `db_path`.
    ///
    /// `dimensions` and `embedding_model` describe the embedding setup and
    /// are persisted on first open; subsequent opens with a different setup
    /// are rejected, since stored vectors would be incomparable.
    pub fn new(
        db_path: &Path,
        dimensions: usize,
        embedding_model: &str,
    ) -> Result<Self, DomainError> {
        let conn = Connection::open(db_path)
            .map_err(|e| DomainError::storage(format!("Failed to open memory database: {e}")))?;
        Self::initialize(conn, dimensions, embedding_model)
    }

    /// In-memory database for tests.
    pub fn in_memory(dimensions: usize, embedding_model: &str) -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            DomainError::storage(format!("Failed to open in-memory memory database: {e}"))
        })?;
        Self::initialize(conn, dimensions, embedding_model)
    }

    fn initialize(
        conn: Connection,
        dimensions: usize,
        embedding_model: &str,
    ) -> Result<Self, DomainError> {
        if dimensions == 0 {
            return Err(DomainError::invalid_input(
                "embedding dimensions must be greater than 0",
            ));
        }
        conn.execute_batch(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS memory_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_items (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                content TEXT NOT NULL,
                source_session_id TEXT,
                project TEXT,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                update_count BIGINT NOT NULL DEFAULT 0,
                UNIQUE (kind, name)
            );
            CREATE TABLE IF NOT EXISTS memory_vectors (
                item_id TEXT PRIMARY KEY,
                vector FLOAT[{dimensions}] NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                imported_at BIGINT NOT NULL,
                message_count BIGINT NOT NULL,
                items_written BIGINT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_nodes (
                uri TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                parent_uri TEXT,
                abstract TEXT NOT NULL,
                overview TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_node_vectors (
                node_uri TEXT PRIMARY KEY,
                vector FLOAT[{dimensions}] NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_dream_runs (
                id TEXT PRIMARY KEY,
                started_at BIGINT NOT NULL,
                finished_at BIGINT NOT NULL,
                sessions_imported BIGINT NOT NULL,
                clusters_found BIGINT NOT NULL,
                operations_applied BIGINT NOT NULL,
                operations_skipped BIGINT NOT NULL,
                status TEXT NOT NULL DEFAULT 'completed'
            );
            "#
        ))
        .map_err(|e| DomainError::storage(format!("Failed to initialize memory schema: {e}")))?;

        // Migration: databases created before per-project memory existed have
        // neither column; those from the earlier `scope` naming have a `scope`
        // column that must be renamed to `project`. Both paths are idempotent.
        conn.execute_batch("ALTER TABLE memory_items ADD COLUMN IF NOT EXISTS project TEXT")
            .map_err(|e| {
                DomainError::storage(format!("Failed to add memory project column: {e}"))
            })?;
        Self::migrate_scope_to_project(&conn)?;

        // Migration: add the dream-run `status` column to databases created
        // before failed runs were tracked. DuckDB rejects a NOT NULL/DEFAULT
        // constraint on ALTER, so the column is added plain and back-filled.
        // Idempotent.
        conn.execute_batch(
            "ALTER TABLE memory_dream_runs ADD COLUMN IF NOT EXISTS status TEXT; \
             UPDATE memory_dream_runs SET status = 'completed' WHERE status IS NULL;",
        )
        .map_err(|e| DomainError::storage(format!("Failed to add dream-run status column: {e}")))?;

        Self::check_meta(&conn, "dimensions", &dimensions.to_string())?;
        Self::check_meta(&conn, "embedding_model", embedding_model)?;

        debug!("memory database schema initialized ({dimensions} dims)");
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            dimensions,
        })
    }

    /// Persist a meta value on first open; reject a mismatch on later opens.
    fn check_meta(conn: &Connection, key: &str, expected: &str) -> Result<(), DomainError> {
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM memory_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                duckdb::Error::QueryReturnedNoRows => Ok(None),
                other => Err(DomainError::storage(format!(
                    "Failed to read memory meta '{key}': {other}"
                ))),
            })?;
        match stored {
            Some(value) if value == expected => Ok(()),
            Some(value) => Err(DomainError::invalid_input(format!(
                "memory database was created with {key}='{value}' but the current configuration \
                 uses '{expected}'; use the original embedding setup or delete the memory \
                 database to start over"
            ))),
            None => {
                conn.execute(
                    "INSERT INTO memory_meta (key, value) VALUES (?1, ?2)",
                    params![key, expected],
                )
                .map_err(|e| {
                    DomainError::storage(format!("Failed to write memory meta '{key}': {e}"))
                })?;
                Ok(())
            }
        }
    }

    /// Fold a legacy `scope` column (the earlier name for `project`) into
    /// `project`, then drop it. A no-op on databases that never had `scope`.
    fn migrate_scope_to_project(conn: &Connection) -> Result<(), DomainError> {
        let has_scope: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_name = 'memory_items' AND column_name = 'scope'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .map_err(|e| {
                DomainError::storage(format!("Failed to inspect memory_items columns: {e}"))
            })?;
        if !has_scope {
            return Ok(());
        }
        conn.execute_batch(
            "UPDATE memory_items SET project = scope WHERE project IS NULL AND scope IS NOT NULL; \
             ALTER TABLE memory_items DROP COLUMN scope;",
        )
        .map_err(|e| {
            DomainError::storage(format!("Failed to migrate scope column to project: {e}"))
        })
    }

    /// Render a vector as a DuckDB `[..]::FLOAT[n]` literal (FLOAT arrays
    /// cannot be bound as parameters).
    fn vector_literal(&self, vector: &[f32]) -> Result<String, DomainError> {
        if vector.len() != self.dimensions {
            return Err(DomainError::invalid_input(format!(
                "vector has {} dimensions, memory database expects {}",
                vector.len(),
                self.dimensions
            )));
        }
        let mut s = String::with_capacity(vector.len() * 8);
        s.push('[');
        for (i, v) in vector.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("{v}"));
        }
        s.push(']');
        s.push_str(&format!("::FLOAT[{}]", self.dimensions));
        Ok(s)
    }

    fn item_from_row(row: &Row<'_>) -> Result<MemoryItem, duckdb::Error> {
        let kind_str: String = row.get(1)?;
        let kind = MemoryKind::parse(&kind_str).unwrap_or(MemoryKind::Fact);
        Ok(MemoryItem::new(
            row.get(0)?,
            kind,
            row.get(2)?,
            row.get(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get::<_, i64>(8)? as u32,
        ))
    }

    fn node_from_row(row: &Row<'_>) -> Result<MemoryNode, duckdb::Error> {
        let kind_str: String = row.get(1)?;
        let kind = NodeKind::parse(&kind_str).unwrap_or(NodeKind::Resource);
        Ok(MemoryNode::new(
            row.get(0)?,
            kind,
            row.get::<_, Option<String>>(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
        ))
    }
}

const ITEM_COLUMNS: &str =
    "id, kind, name, content, source_session_id, project, created_at, updated_at, update_count";

const NODE_COLUMNS: &str =
    "uri, kind, parent_uri, abstract, overview, content, created_at, updated_at";

#[async_trait]
impl MemoryRepository for DuckdbMemoryRepository {
    async fn upsert_item(
        &self,
        item: &MemoryItem,
        vector: Option<&[f32]>,
    ) -> Result<(), DomainError> {
        let vector_literal = vector.map(|v| self.vector_literal(v)).transpose()?;
        let conn = self.conn.lock().await;

        // Replace any previous item with the same identity (by id or by the
        // (kind, name) key) so both unique constraints stay conflict-free.
        conn.execute(
            "DELETE FROM memory_vectors WHERE item_id IN \
             (SELECT id FROM memory_items WHERE id = ?1 OR (kind = ?2 AND name = ?3))",
            params![item.id(), item.kind().as_str(), item.name()],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear memory vector: {e}")))?;
        conn.execute(
            "DELETE FROM memory_items WHERE id = ?1 OR (kind = ?2 AND name = ?3)",
            params![item.id(), item.kind().as_str(), item.name()],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear memory item: {e}")))?;

        conn.execute(
            &format!(
                "INSERT INTO memory_items ({ITEM_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            ),
            params![
                item.id(),
                item.kind().as_str(),
                item.name(),
                item.content(),
                item.source_session_id(),
                item.project(),
                item.created_at(),
                item.updated_at(),
                item.update_count() as i64,
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to insert memory item: {e}")))?;

        if let Some(literal) = vector_literal {
            conn.execute(
                &format!("INSERT INTO memory_vectors (item_id, vector) VALUES (?1, {literal})"),
                params![item.id()],
            )
            .map_err(|e| DomainError::storage(format!("Failed to insert memory vector: {e}")))?;
        }
        Ok(())
    }

    async fn find_item(
        &self,
        kind: MemoryKind,
        name: &str,
    ) -> Result<Option<MemoryItem>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM memory_items WHERE kind = ?1 AND name = ?2"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare find_item: {e}")))?;
        match stmt.query_row(params![kind.as_str(), name], Self::item_from_row) {
            Ok(item) => Ok(Some(item)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!(
                "Failed to query memory item: {e}"
            ))),
        }
    }

    async fn find_item_by_id(&self, id: &str) -> Result<Option<MemoryItem>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM memory_items WHERE id = ?1"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare find_item_by_id: {e}")))?;
        match stmt.query_row(params![id], Self::item_from_row) {
            Ok(item) => Ok(Some(item)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!(
                "Failed to query memory item by id: {e}"
            ))),
        }
    }

    async fn delete_item(&self, kind: MemoryKind, name: &str) -> Result<bool, DomainError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM memory_vectors WHERE item_id IN \
             (SELECT id FROM memory_items WHERE kind = ?1 AND name = ?2)",
            params![kind.as_str(), name],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete memory vector: {e}")))?;
        let deleted = conn
            .execute(
                "DELETE FROM memory_items WHERE kind = ?1 AND name = ?2",
                params![kind.as_str(), name],
            )
            .map_err(|e| DomainError::storage(format!("Failed to delete memory item: {e}")))?;
        Ok(deleted > 0)
    }

    async fn delete_item_by_id(&self, id: &str) -> Result<bool, DomainError> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM memory_vectors WHERE item_id = ?1", params![id])
            .map_err(|e| DomainError::storage(format!("Failed to delete memory vector: {e}")))?;
        let deleted = conn
            .execute("DELETE FROM memory_items WHERE id = ?1", params![id])
            .map_err(|e| DomainError::storage(format!("Failed to delete memory item: {e}")))?;
        Ok(deleted > 0)
    }

    async fn list_items(&self, kind: Option<MemoryKind>) -> Result<Vec<MemoryItem>, DomainError> {
        let conn = self.conn.lock().await;
        let (sql, kind_param) = match kind {
            Some(k) => (
                format!(
                    "SELECT {ITEM_COLUMNS} FROM memory_items WHERE kind = ?1 \
                     ORDER BY updated_at DESC, name"
                ),
                Some(k.as_str().to_string()),
            ),
            None => (
                format!("SELECT {ITEM_COLUMNS} FROM memory_items ORDER BY updated_at DESC, name"),
                None,
            ),
        };
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare list_items: {e}")))?;
        let rows = match kind_param {
            Some(k) => stmt.query_map(params![k], Self::item_from_row),
            None => stmt.query_map([], Self::item_from_row),
        }
        .map_err(|e| DomainError::storage(format!("Failed to list memory items: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read memory item row: {e}")))
    }

    async fn search_semantic(
        &self,
        vector: &[f32],
        kind: Option<MemoryKind>,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(MemoryItem, f32)>, DomainError> {
        let literal = self.vector_literal(vector)?;
        let mut conditions: Vec<String> = Vec::new();
        if let Some(k) = kind {
            conditions.push(format!("i.kind = '{}'", k.as_str()));
        }
        if let Some(p) = project {
            conditions.push(format!(
                "(i.project IS NULL OR i.project = '{}')",
                sql_quote(p)
            ));
        }
        let kind_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let sql = format!(
            "SELECT {cols}, 1.0 - array_cosine_distance(v.vector, {literal}) AS score \
             FROM memory_items i \
             JOIN memory_vectors v ON v.item_id = i.id \
             {kind_clause} \
             ORDER BY score DESC \
             LIMIT {limit}",
            cols = ITEM_COLUMNS
                .split(", ")
                .map(|c| format!("i.{c}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare semantic search: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let item = Self::item_from_row(row)?;
                // Score is the column appended after ITEM_COLUMNS' 9 fields.
                let score: f32 = row.get(ITEM_COLUMNS.split(", ").count())?;
                Ok((item, score))
            })
            .map_err(|e| DomainError::storage(format!("Semantic memory search failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read semantic search row: {e}")))
    }

    async fn search_keyword(
        &self,
        query: &str,
        kind: Option<MemoryKind>,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(MemoryItem, f32)>, DomainError> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .take(16)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        // Score = fraction of query terms found in name or content.
        let escape = |t: &str| {
            t.replace('\\', "\\\\")
                .replace('\'', "''")
                .replace('%', "\\%")
                .replace('_', "\\_")
        };
        let match_cases: Vec<String> = terms
            .iter()
            .map(|t| {
                let e = escape(t);
                format!(
                    "(CASE WHEN lower(name) LIKE '%{e}%' ESCAPE '\\' \
                       OR lower(content) LIKE '%{e}%' ESCAPE '\\' THEN 1 ELSE 0 END)"
                )
            })
            .collect();
        let score_expr = format!("({}) / {}.0", match_cases.join(" + "), terms.len());
        let mut kind_clause = match kind {
            Some(k) => format!("AND kind = '{}'", k.as_str()),
            None => String::new(),
        };
        if let Some(p) = project {
            kind_clause.push_str(&format!(
                " AND (project IS NULL OR project = '{}')",
                sql_quote(p)
            ));
        }
        let sql = format!(
            "SELECT {ITEM_COLUMNS}, {score_expr} AS score \
             FROM memory_items \
             WHERE {score_expr} > 0 {kind_clause} \
             ORDER BY score DESC, updated_at DESC \
             LIMIT {limit}"
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare keyword search: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let item = Self::item_from_row(row)?;
                // Score is the column appended after ITEM_COLUMNS' 9 fields.
                let score: f64 = row.get(ITEM_COLUMNS.split(", ").count())?;
                Ok((item, score as f32))
            })
            .map_err(|e| DomainError::storage(format!("Keyword memory search failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read keyword search row: {e}")))
    }

    async fn list_item_vectors(&self) -> Result<Vec<(String, Vec<f32>)>, DomainError> {
        let conn = self.conn.clone();
        // The full vector table is scanned and every row JSON-decoded here, so
        // this must not run on a Tokio worker thread.
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // FLOAT[n] values cannot be fetched as a native Rust type through
            // duckdb-rs, so round-trip them through JSON text.
            let mut stmt = conn
                .prepare("SELECT item_id, to_json(vector)::VARCHAR FROM memory_vectors")
                .map_err(|e| {
                    DomainError::storage(format!("Failed to prepare list_item_vectors: {e}"))
                })?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| DomainError::storage(format!("Failed to list item vectors: {e}")))?;
            let mut vectors = Vec::new();
            for row in rows {
                let (item_id, json) = row
                    .map_err(|e| DomainError::storage(format!("Failed to read vector row: {e}")))?;
                let vector: Vec<f32> = serde_json::from_str(&json).map_err(|e| {
                    DomainError::storage(format!("Failed to parse vector for '{item_id}': {e}"))
                })?;
                vectors.push((item_id, vector));
            }
            Ok(vectors)
        })
        .await
        .map_err(|e| DomainError::storage(format!("Blocking task panicked: {e}")))?
    }

    async fn find_item_vector(&self, id: &str) -> Result<Option<Vec<f32>>, DomainError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT to_json(vector)::VARCHAR FROM memory_vectors WHERE item_id = ?1")
                .map_err(|e| {
                    DomainError::storage(format!("Failed to prepare find_item_vector: {e}"))
                })?;
            match stmt.query_row(params![id], |row| row.get::<_, String>(0)) {
                Ok(json) => {
                    let vector: Vec<f32> = serde_json::from_str(&json).map_err(|e| {
                        DomainError::storage(format!("Failed to parse vector for '{id}': {e}"))
                    })?;
                    Ok(Some(vector))
                }
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(DomainError::storage(format!(
                    "Failed to query item vector: {e}"
                ))),
            }
        })
        .await
        .map_err(|e| DomainError::storage(format!("Blocking task panicked: {e}")))?
    }

    async fn record_session(&self, session: &ImportedSession) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO memory_sessions (id, source, imported_at, message_count, items_written) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (id) DO UPDATE SET \
                 source = excluded.source, \
                 imported_at = excluded.imported_at, \
                 message_count = excluded.message_count, \
                 items_written = excluded.items_written",
            params![
                session.id,
                session.source,
                session.imported_at,
                session.message_count as i64,
                session.items_written as i64,
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to record session: {e}")))?;
        Ok(())
    }

    async fn find_session(&self, id: &str) -> Result<Option<ImportedSession>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, source, imported_at, message_count, items_written \
                 FROM memory_sessions WHERE id = ?1",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare find_session: {e}")))?;
        match stmt.query_row(params![id], session_from_row) {
            Ok(session) => Ok(Some(session)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!(
                "Failed to query session: {e}"
            ))),
        }
    }

    async fn list_sessions(&self) -> Result<Vec<ImportedSession>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, source, imported_at, message_count, items_written \
                 FROM memory_sessions ORDER BY imported_at DESC",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare list_sessions: {e}")))?;
        let rows = stmt
            .query_map([], session_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to list sessions: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read session row: {e}")))
    }

    async fn upsert_node(
        &self,
        node: &MemoryNode,
        vector: Option<&[f32]>,
    ) -> Result<(), DomainError> {
        let vector_literal = vector.map(|v| self.vector_literal(v)).transpose()?;
        let conn = self.conn.lock().await;

        // Replace any previous node with the same URI so both tables stay
        // conflict-free (URI is the primary key on each).
        conn.execute(
            "DELETE FROM memory_node_vectors WHERE node_uri = ?1",
            params![node.uri()],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear node vector: {e}")))?;
        conn.execute(
            "DELETE FROM memory_nodes WHERE uri = ?1",
            params![node.uri()],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear node: {e}")))?;

        conn.execute(
            &format!(
                "INSERT INTO memory_nodes ({NODE_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            params![
                node.uri(),
                node.kind().as_str(),
                node.parent_uri(),
                node.abstract_(),
                node.overview(),
                node.content(),
                node.created_at(),
                node.updated_at(),
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to insert node: {e}")))?;

        if let Some(literal) = vector_literal {
            conn.execute(
                &format!(
                    "INSERT INTO memory_node_vectors (node_uri, vector) VALUES (?1, {literal})"
                ),
                params![node.uri()],
            )
            .map_err(|e| DomainError::storage(format!("Failed to insert node vector: {e}")))?;
        }
        Ok(())
    }

    async fn find_node(&self, uri: &str) -> Result<Option<MemoryNode>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {NODE_COLUMNS} FROM memory_nodes WHERE uri = ?1"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare find_node: {e}")))?;
        match stmt.query_row(params![uri], Self::node_from_row) {
            Ok(node) => Ok(Some(node)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!("Failed to query node: {e}"))),
        }
    }

    async fn delete_node(&self, uri: &str) -> Result<bool, DomainError> {
        let conn = self.conn.clone();
        let uri = uri.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM memory_node_vectors WHERE node_uri = ?1",
                params![&uri],
            )
            .map_err(|e| DomainError::storage(format!("Failed to delete node vector: {e}")))?;
            let deleted = conn
                .execute("DELETE FROM memory_nodes WHERE uri = ?1", params![&uri])
                .map_err(|e| DomainError::storage(format!("Failed to delete node: {e}")))?;
            Ok(deleted > 0)
        })
        .await
        .map_err(|e| DomainError::storage(format!("Blocking task panicked: {e}")))?
    }

    async fn list_child_nodes(&self, parent_uri: &str) -> Result<Vec<MemoryNode>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {NODE_COLUMNS} FROM memory_nodes WHERE parent_uri = ?1 \
                 ORDER BY updated_at DESC, uri"
            ))
            .map_err(|e| {
                DomainError::storage(format!("Failed to prepare list_child_nodes: {e}"))
            })?;
        let rows = stmt
            .query_map(params![parent_uri], Self::node_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to list child nodes: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read node row: {e}")))
    }

    async fn list_nodes(&self, kind: Option<NodeKind>) -> Result<Vec<MemoryNode>, DomainError> {
        let conn = self.conn.lock().await;
        let (sql, kind_param) = match kind {
            Some(k) => (
                format!(
                    "SELECT {NODE_COLUMNS} FROM memory_nodes WHERE kind = ?1 \
                     ORDER BY updated_at DESC, uri"
                ),
                Some(k.as_str().to_string()),
            ),
            None => (
                format!("SELECT {NODE_COLUMNS} FROM memory_nodes ORDER BY updated_at DESC, uri"),
                None,
            ),
        };
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare list_nodes: {e}")))?;
        let rows = match kind_param {
            Some(k) => stmt.query_map(params![k], Self::node_from_row),
            None => stmt.query_map([], Self::node_from_row),
        }
        .map_err(|e| DomainError::storage(format!("Failed to list nodes: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read node row: {e}")))
    }

    async fn search_nodes_semantic(
        &self,
        vector: &[f32],
        kind: Option<NodeKind>,
        limit: usize,
    ) -> Result<Vec<(MemoryNode, f32)>, DomainError> {
        let literal = self.vector_literal(vector)?;
        let kind_clause = match kind {
            Some(k) => format!("WHERE n.kind = '{}'", k.as_str()),
            None => String::new(),
        };
        let sql = format!(
            "SELECT {cols}, 1.0 - array_cosine_distance(v.vector, {literal}) AS score \
             FROM memory_nodes n \
             JOIN memory_node_vectors v ON v.node_uri = n.uri \
             {kind_clause} \
             ORDER BY score DESC \
             LIMIT {limit}",
            cols = NODE_COLUMNS
                .split(", ")
                .map(|c| format!("n.{c}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare node semantic search: {e}"))
        })?;
        let rows = stmt
            .query_map([], |row| {
                let node = Self::node_from_row(row)?;
                let score: f32 = row.get(8)?;
                Ok((node, score))
            })
            .map_err(|e| DomainError::storage(format!("Node semantic search failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read node search row: {e}")))
    }

    async fn search_nodes_keyword(
        &self,
        query: &str,
        kind: Option<NodeKind>,
        limit: usize,
    ) -> Result<Vec<(MemoryNode, f32)>, DomainError> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .take(16)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let escape = |t: &str| {
            t.replace('\\', "\\\\")
                .replace('\'', "''")
                .replace('%', "\\%")
                .replace('_', "\\_")
        };
        // Score = fraction of query terms found in abstract or overview.
        let match_cases: Vec<String> = terms
            .iter()
            .map(|t| {
                let e = escape(t);
                format!(
                    "(CASE WHEN lower(abstract) LIKE '%{e}%' ESCAPE '\\' \
                       OR lower(overview) LIKE '%{e}%' ESCAPE '\\' THEN 1 ELSE 0 END)"
                )
            })
            .collect();
        let score_expr = format!("({}) / {}.0", match_cases.join(" + "), terms.len());
        let kind_clause = match kind {
            Some(k) => format!("AND kind = '{}'", k.as_str()),
            None => String::new(),
        };
        let sql = format!(
            "SELECT {NODE_COLUMNS}, {score_expr} AS score \
             FROM memory_nodes \
             WHERE {score_expr} > 0 {kind_clause} \
             ORDER BY score DESC, updated_at DESC \
             LIMIT {limit}"
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare node keyword search: {e}"))
        })?;
        let rows = stmt
            .query_map([], |row| {
                let node = Self::node_from_row(row)?;
                let score: f64 = row.get(8)?;
                Ok((node, score as f32))
            })
            .map_err(|e| DomainError::storage(format!("Node keyword search failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read node search row: {e}")))
    }

    async fn record_dream_run(&self, run: &DreamRun) -> Result<(), DomainError> {
        let conn = self.conn.clone();
        let run = run.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO memory_dream_runs \
                 (id, started_at, finished_at, sessions_imported, clusters_found, \
                  operations_applied, operations_skipped, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (id) DO UPDATE SET \
                     started_at = excluded.started_at, \
                     finished_at = excluded.finished_at, \
                     sessions_imported = excluded.sessions_imported, \
                     clusters_found = excluded.clusters_found, \
                     operations_applied = excluded.operations_applied, \
                     operations_skipped = excluded.operations_skipped, \
                     status = excluded.status",
                params![
                    run.id,
                    run.started_at,
                    run.finished_at,
                    run.sessions_imported as i64,
                    run.clusters_found as i64,
                    run.operations_applied as i64,
                    run.operations_skipped as i64,
                    run.status,
                ],
            )
            .map_err(|e| DomainError::storage(format!("Failed to record dream run: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| DomainError::storage(format!("Blocking task panicked: {e}")))?
    }

    async fn last_dream_run(&self) -> Result<Option<DreamRun>, DomainError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, started_at, finished_at, sessions_imported, clusters_found, \
                            operations_applied, operations_skipped, status \
                     FROM memory_dream_runs ORDER BY finished_at DESC LIMIT 1",
                )
                .map_err(|e| {
                    DomainError::storage(format!("Failed to prepare last_dream_run: {e}"))
                })?;
            match stmt.query_row([], dream_run_from_row) {
                Ok(run) => Ok(Some(run)),
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(DomainError::storage(format!(
                    "Failed to query dream run: {e}"
                ))),
            }
        })
        .await
        .map_err(|e| DomainError::storage(format!("Blocking task panicked: {e}")))?
    }

    async fn stats(&self) -> Result<MemoryStats, DomainError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            // Count items by kind
            let mut items_by_kind: Vec<(String, u64)> = Vec::new();
            for kind in MemoryKind::ALL {
                let kind_str = kind.as_str();
                let sql = format!("SELECT COUNT(*) FROM memory_items WHERE kind = '{kind_str}'");
                let count: u64 = conn.query_row(&sql, [], |row| row.get(0)).unwrap_or(0);
                items_by_kind.push((kind_str.to_string(), count));
            }
            let total_items: u64 = items_by_kind.iter().map(|(_, c)| c).sum();

            // Count sessions
            let total_sessions: u64 = conn
                .query_row("SELECT COUNT(*) FROM memory_sessions", [], |row| row.get(0))
                .unwrap_or(0);

            // Count nodes by kind
            let mut nodes_by_kind: Vec<(String, u64)> = Vec::new();
            for kind in NodeKind::ALL {
                let kind_str = kind.as_str();
                let sql = format!("SELECT COUNT(*) FROM memory_nodes WHERE kind = '{kind_str}'");
                let count: u64 = conn.query_row(&sql, [], |row| row.get(0)).unwrap_or(0);
                nodes_by_kind.push((kind_str.to_string(), count));
            }
            let total_nodes: u64 = nodes_by_kind.iter().map(|(_, c)| c).sum();

            Ok(MemoryStats {
                total_items,
                items_by_kind,
                total_sessions,
                total_nodes,
                nodes_by_kind,
            })
        })
        .await
        .map_err(|e| DomainError::storage(format!("Blocking task panicked: {e}")))?
    }
}

/// Escape a string for interpolation into a single-quoted SQL literal.
fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn dream_run_from_row(row: &Row<'_>) -> Result<DreamRun, duckdb::Error> {
    Ok(DreamRun {
        id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        sessions_imported: row.get::<_, i64>(3)? as usize,
        clusters_found: row.get::<_, i64>(4)? as usize,
        operations_applied: row.get::<_, i64>(5)? as usize,
        operations_skipped: row.get::<_, i64>(6)? as usize,
        status: row.get(7)?,
    })
}

fn session_from_row(row: &Row<'_>) -> Result<ImportedSession, duckdb::Error> {
    Ok(ImportedSession {
        id: row.get(0)?,
        source: row.get(1)?,
        imported_at: row.get(2)?,
        message_count: row.get::<_, i64>(3)? as usize,
        items_written: row.get::<_, i64>(4)? as usize,
    })
}
