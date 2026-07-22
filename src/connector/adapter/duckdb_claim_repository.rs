//! DuckDB-backed [`ClaimRepository`] — the experimental append-only claim graph.
//!
//! Lives in its own database file (`memory-claims.duckdb`), separate from both
//! the code index and the current `memory.duckdb`, so the claim experiment can
//! be inspected or wiped independently and never contends with either. The
//! layout mirrors the existing memory adapter: a `*_meta` guard on the embedding
//! setup, brute-force `array_cosine_distance` vector search, and the same
//! single-connection-behind-a-`Mutex` pattern.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection, Row};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::ClaimRepository;
use crate::domain::{
    Claim, ClaimEdge, ClaimStatus, ClaimStoreStats, DomainError, EdgeOrigin, EdgeType, Entity,
    EntityRef, SourceKind,
};

/// File name of the claim-graph database inside the data directory.
pub const CLAIM_DB_FILE: &str = "memory-claims.duckdb";

/// Claim columns, in the order [`claim_from_row`] reads them.
const CLAIM_COLUMNS: &str = "id, subject_entity_id, subject_literal, predicate, \
    object_entity_id, object_literal, statement, project, recorded_at, valid_from, \
    valid_to, source_session_id, source_message_index, source_kind, confidence, \
    status, derived, derived_from";

/// Entity base columns (aliases are hydrated separately).
const ENTITY_COLUMNS: &str = "id, entity_type, canonical_name, created_at, updated_at";

/// Edge columns, in the order [`edge_from_row`] reads them.
const EDGE_COLUMNS: &str = "from_claim, to_claim, edge_type, created_at, created_by, confidence";

pub struct DuckdbClaimRepository {
    conn: Arc<Mutex<Connection>>,
    dimensions: usize,
}

impl DuckdbClaimRepository {
    /// Open (or create) the claim database at `db_path`.
    ///
    /// `dimensions` / `embedding_model` describe the embedding setup and are
    /// persisted on first open; a later open with a different setup is rejected,
    /// since stored vectors would be incomparable.
    pub fn new(
        db_path: &Path,
        dimensions: usize,
        embedding_model: &str,
    ) -> Result<Self, DomainError> {
        let conn = Connection::open(db_path)
            .map_err(|e| DomainError::storage(format!("Failed to open claim database: {e}")))?;
        Self::initialize(conn, dimensions, embedding_model)
    }

    /// In-memory database for tests.
    pub fn in_memory(dimensions: usize, embedding_model: &str) -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            DomainError::storage(format!("Failed to open in-memory claim database: {e}"))
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
            CREATE TABLE IF NOT EXISTS claim_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS claims (
                id TEXT PRIMARY KEY,
                subject_entity_id TEXT,
                subject_literal TEXT,
                predicate TEXT NOT NULL,
                object_entity_id TEXT,
                object_literal TEXT,
                statement TEXT NOT NULL,
                project TEXT,
                recorded_at BIGINT NOT NULL,
                valid_from BIGINT NOT NULL,
                valid_to BIGINT,
                source_session_id TEXT,
                source_message_index BIGINT,
                source_kind TEXT NOT NULL,
                confidence DOUBLE NOT NULL,
                status TEXT NOT NULL,
                derived BOOLEAN NOT NULL,
                derived_from TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE IF NOT EXISTS claim_vectors (
                claim_id TEXT PRIMARY KEY,
                vector FLOAT[{dimensions}] NOT NULL
            );
            CREATE TABLE IF NOT EXISTS entities (
                id TEXT PRIMARY KEY,
                entity_type TEXT NOT NULL,
                canonical_name TEXT NOT NULL,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS entity_aliases (
                alias TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                PRIMARY KEY (alias, entity_id)
            );
            CREATE TABLE IF NOT EXISTS entity_vectors (
                entity_id TEXT PRIMARY KEY,
                vector FLOAT[{dimensions}] NOT NULL
            );
            CREATE TABLE IF NOT EXISTS claim_edges (
                from_claim TEXT NOT NULL,
                to_claim TEXT NOT NULL,
                edge_type TEXT NOT NULL,
                created_at BIGINT NOT NULL,
                created_by TEXT NOT NULL,
                confidence DOUBLE NOT NULL,
                PRIMARY KEY (from_claim, to_claim, edge_type)
            );
            "#
        ))
        .map_err(|e| DomainError::storage(format!("Failed to initialize claim schema: {e}")))?;

        Self::check_meta(&conn, "dimensions", &dimensions.to_string())?;
        Self::check_meta(&conn, "embedding_model", embedding_model)?;

        debug!("claim database schema initialized ({dimensions} dims)");
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            dimensions,
        })
    }

    /// Store `expected` under `key` on first open; on later opens reject a
    /// mismatch (stored vectors would be incomparable with new queries).
    fn check_meta(conn: &Connection, key: &str, expected: &str) -> Result<(), DomainError> {
        let existing: Option<String> = conn
            .query_row(
                "SELECT value FROM claim_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .ok();
        match existing {
            Some(found) if found == expected => Ok(()),
            Some(found) => Err(DomainError::invalid_input(format!(
                "claim store {key} mismatch: stored '{found}', requested '{expected}'"
            ))),
            None => {
                conn.execute(
                    "INSERT INTO claim_meta (key, value) VALUES (?1, ?2)",
                    params![key, expected],
                )
                .map_err(|e| DomainError::storage(format!("Failed to write claim meta: {e}")))?;
                Ok(())
            }
        }
    }

    /// Render a vector as a DuckDB `FLOAT[dims]` literal, validating dimension.
    fn vector_literal(&self, vector: &[f32]) -> Result<String, DomainError> {
        if vector.len() != self.dimensions {
            return Err(DomainError::invalid_input(format!(
                "vector has {} dimensions, claim database expects {}",
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

    /// Claim columns prefixed with a table alias, e.g. `c.id, c.subject_entity_id, …`.
    fn prefixed_claim_columns(alias: &str) -> String {
        CLAIM_COLUMNS
            .split(", ")
            .map(|c| format!("{alias}.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn claim_from_row(row: &Row<'_>) -> Result<Claim, duckdb::Error> {
        let subject = EntityRef::from_columns(
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        );
        let object = EntityRef::from_columns(
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        );
        let source_kind =
            SourceKind::parse(&row.get::<_, String>(13)?).unwrap_or(SourceKind::AssistantInferred);
        let status = ClaimStatus::parse(&row.get::<_, String>(15)?).unwrap_or(ClaimStatus::Active);
        let derived_from: Vec<String> =
            serde_json::from_str(&row.get::<_, String>(17)?).unwrap_or_default();
        Ok(Claim {
            id: row.get(0)?,
            subject,
            predicate: row.get(3)?,
            object,
            statement: row.get(6)?,
            project: row.get::<_, Option<String>>(7)?,
            recorded_at: row.get(8)?,
            valid_from: row.get(9)?,
            valid_to: row.get::<_, Option<i64>>(10)?,
            source_session_id: row.get::<_, Option<String>>(11)?,
            source_message_index: row.get::<_, Option<i64>>(12)?,
            source_kind,
            confidence: row.get::<_, f64>(14)? as f32,
            status,
            derived: row.get::<_, bool>(16)?,
            derived_from,
        })
    }

    fn edge_from_row(row: &Row<'_>) -> Result<ClaimEdge, duckdb::Error> {
        let edge_type = EdgeType::parse(&row.get::<_, String>(2)?).unwrap_or(EdgeType::RelatesTo);
        let created_by =
            EdgeOrigin::parse(&row.get::<_, String>(4)?).unwrap_or(EdgeOrigin::Ingestion);
        Ok(ClaimEdge {
            from_claim: row.get(0)?,
            to_claim: row.get(1)?,
            edge_type,
            created_at: row.get(3)?,
            created_by,
            confidence: row.get::<_, f64>(5)? as f32,
        })
    }

    /// Aliases attached to `entity_id`, sorted for determinism.
    fn aliases_for(conn: &Connection, entity_id: &str) -> Result<Vec<String>, DomainError> {
        let mut stmt = conn
            .prepare("SELECT alias FROM entity_aliases WHERE entity_id = ?1 ORDER BY alias")
            .map_err(|e| DomainError::storage(format!("Failed to prepare aliases query: {e}")))?;
        let rows = stmt
            .query_map(params![entity_id], |row| row.get::<_, String>(0))
            .map_err(|e| DomainError::storage(format!("Failed to query aliases: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read alias row: {e}")))
    }

    /// Read an entity's base columns from a row (aliases hydrated by the caller).
    fn entity_base_from_row(row: &Row<'_>) -> Result<Entity, duckdb::Error> {
        Ok(Entity {
            id: row.get(0)?,
            entity_type: row.get(1)?,
            canonical_name: row.get(2)?,
            aliases: Vec::new(),
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
        })
    }
}

/// Split an [`EntityRef`] into the `(entity_id, literal)` column pair.
fn ref_columns(r: &EntityRef) -> (Option<&str>, Option<&str>) {
    (r.entity_id(), r.literal())
}

#[async_trait]
impl ClaimRepository for DuckdbClaimRepository {
    async fn append_claim(&self, claim: &Claim, vector: Option<&[f32]>) -> Result<(), DomainError> {
        let vector_literal = vector.map(|v| self.vector_literal(v)).transpose()?;
        let (subject_entity_id, subject_literal) = ref_columns(&claim.subject);
        let (object_entity_id, object_literal) = ref_columns(&claim.object);
        let derived_from =
            serde_json::to_string(&claim.derived_from).unwrap_or_else(|_| "[]".to_string());

        let conn = self.conn.lock().await;
        // Appending a claim whose id already exists replaces it (idempotent
        // re-append); ingestion always mints a fresh id, so this is a no-op in
        // the normal path.
        conn.execute(
            "DELETE FROM claim_vectors WHERE claim_id = ?1",
            params![claim.id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear claim vector: {e}")))?;
        conn.execute("DELETE FROM claims WHERE id = ?1", params![claim.id])
            .map_err(|e| DomainError::storage(format!("Failed to clear claim: {e}")))?;

        conn.execute(
            &format!(
                "INSERT INTO claims ({CLAIM_COLUMNS}) VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"
            ),
            params![
                claim.id,
                subject_entity_id,
                subject_literal,
                claim.predicate,
                object_entity_id,
                object_literal,
                claim.statement,
                claim.project.as_deref(),
                claim.recorded_at,
                claim.valid_from,
                claim.valid_to,
                claim.source_session_id.as_deref(),
                claim.source_message_index,
                claim.source_kind.as_str(),
                claim.confidence as f64,
                claim.status.as_str(),
                claim.derived,
                derived_from,
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to insert claim: {e}")))?;

        if let Some(literal) = vector_literal {
            conn.execute(
                &format!("INSERT INTO claim_vectors (claim_id, vector) VALUES (?1, {literal})"),
                params![claim.id],
            )
            .map_err(|e| DomainError::storage(format!("Failed to insert claim vector: {e}")))?;
        }
        Ok(())
    }

    async fn find_claim(&self, id: &str) -> Result<Option<Claim>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!("SELECT {CLAIM_COLUMNS} FROM claims WHERE id = ?1"))
            .map_err(|e| DomainError::storage(format!("Failed to prepare find_claim: {e}")))?;
        let mut rows = stmt
            .query_map(params![id], Self::claim_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to query claim: {e}")))?;
        match rows.next() {
            Some(row) => Ok(Some(row.map_err(|e| {
                DomainError::storage(format!("Failed to read claim: {e}"))
            })?)),
            None => Ok(None),
        }
    }

    async fn list_claims(
        &self,
        status: Option<ClaimStatus>,
        project: Option<&str>,
    ) -> Result<Vec<Claim>, DomainError> {
        let mut conditions: Vec<String> = Vec::new();
        if let Some(s) = status {
            conditions.push(format!("status = '{}'", s.as_str()));
        }
        if let Some(p) = project {
            conditions.push(format!("(project IS NULL OR project = '{}')", sql_quote(p)));
        }
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let sql = format!(
            "SELECT {CLAIM_COLUMNS} FROM claims {where_clause} ORDER BY recorded_at DESC, id"
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare list_claims: {e}")))?;
        let rows = stmt
            .query_map([], Self::claim_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to list claims: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read claim row: {e}")))
    }

    async fn set_claim_status(
        &self,
        id: &str,
        status: ClaimStatus,
        valid_to: Option<i64>,
    ) -> Result<bool, DomainError> {
        let conn = self.conn.lock().await;
        // Only touch `valid_to` when the caller supplies one, so retracting a
        // claim doesn't silently clear a window set by an earlier supersession.
        let updated = match valid_to {
            Some(ts) => conn.execute(
                "UPDATE claims SET status = ?2, valid_to = ?3 WHERE id = ?1",
                params![id, status.as_str(), ts],
            ),
            None => conn.execute(
                "UPDATE claims SET status = ?2 WHERE id = ?1",
                params![id, status.as_str()],
            ),
        }
        .map_err(|e| DomainError::storage(format!("Failed to update claim status: {e}")))?;
        Ok(updated > 0)
    }

    async fn delete_claims_for_session(&self, session_id: &str) -> Result<usize, DomainError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM claim_vectors WHERE claim_id IN \
             (SELECT id FROM claims WHERE source_session_id = ?1)",
            params![session_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear session claim vectors: {e}")))?;
        conn.execute(
            "DELETE FROM claim_edges WHERE \
             from_claim IN (SELECT id FROM claims WHERE source_session_id = ?1) OR \
             to_claim IN (SELECT id FROM claims WHERE source_session_id = ?1)",
            params![session_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear session claim edges: {e}")))?;
        let deleted = conn
            .execute(
                "DELETE FROM claims WHERE source_session_id = ?1",
                params![session_id],
            )
            .map_err(|e| DomainError::storage(format!("Failed to delete session claims: {e}")))?;
        Ok(deleted)
    }

    async fn add_edge(&self, edge: &ClaimEdge) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM claim_edges WHERE from_claim = ?1 AND to_claim = ?2 AND edge_type = ?3",
            params![edge.from_claim, edge.to_claim, edge.edge_type.as_str()],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear edge: {e}")))?;
        conn.execute(
            &format!("INSERT INTO claim_edges ({EDGE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
            params![
                edge.from_claim,
                edge.to_claim,
                edge.edge_type.as_str(),
                edge.created_at,
                edge.created_by.as_str(),
                edge.confidence as f64,
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to insert edge: {e}")))?;
        Ok(())
    }

    async fn edges_from(&self, claim_id: &str) -> Result<Vec<ClaimEdge>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {EDGE_COLUMNS} FROM claim_edges WHERE from_claim = ?1 ORDER BY created_at"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare edges_from: {e}")))?;
        let rows = stmt
            .query_map(params![claim_id], Self::edge_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to query edges_from: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read edge row: {e}")))
    }

    async fn edges_to(&self, claim_id: &str) -> Result<Vec<ClaimEdge>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {EDGE_COLUMNS} FROM claim_edges WHERE to_claim = ?1 ORDER BY created_at"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare edges_to: {e}")))?;
        let rows = stmt
            .query_map(params![claim_id], Self::edge_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to query edges_to: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read edge row: {e}")))
    }

    async fn upsert_entity(
        &self,
        entity: &Entity,
        vector: Option<&[f32]>,
    ) -> Result<(), DomainError> {
        let vector_literal = vector.map(|v| self.vector_literal(v)).transpose()?;
        // Dedupe aliases and drop blanks so the (alias, entity_id) key never
        // collides within one entity. Seed the seen-set with the canonical name
        // so an alias identical to it (case-insensitively) is redundant and
        // dropped — resolution already matches the canonical name directly.
        let mut seen = std::collections::HashSet::new();
        seen.insert(entity.canonical_name.trim().to_lowercase());
        let aliases: Vec<&str> = entity
            .aliases
            .iter()
            .map(|a| a.trim())
            .filter(|a| !a.is_empty() && seen.insert(a.to_lowercase()))
            .collect();

        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM entity_vectors WHERE entity_id = ?1",
            params![entity.id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear entity vector: {e}")))?;
        conn.execute(
            "DELETE FROM entity_aliases WHERE entity_id = ?1",
            params![entity.id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear entity aliases: {e}")))?;
        conn.execute("DELETE FROM entities WHERE id = ?1", params![entity.id])
            .map_err(|e| DomainError::storage(format!("Failed to clear entity: {e}")))?;

        conn.execute(
            &format!("INSERT INTO entities ({ENTITY_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5)"),
            params![
                entity.id,
                entity.entity_type,
                entity.canonical_name,
                entity.created_at,
                entity.updated_at,
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to insert entity: {e}")))?;

        for alias in aliases {
            conn.execute(
                "INSERT INTO entity_aliases (alias, entity_id) VALUES (?1, ?2)",
                params![alias, entity.id],
            )
            .map_err(|e| DomainError::storage(format!("Failed to insert entity alias: {e}")))?;
        }

        if let Some(literal) = vector_literal {
            conn.execute(
                &format!("INSERT INTO entity_vectors (entity_id, vector) VALUES (?1, {literal})"),
                params![entity.id],
            )
            .map_err(|e| DomainError::storage(format!("Failed to insert entity vector: {e}")))?;
        }
        Ok(())
    }

    async fn find_entity(&self, id: &str) -> Result<Option<Entity>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ENTITY_COLUMNS} FROM entities WHERE id = ?1"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare find_entity: {e}")))?;
        let mut rows = stmt
            .query_map(params![id], Self::entity_base_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to query entity: {e}")))?;
        match rows.next() {
            Some(row) => {
                let mut entity =
                    row.map_err(|e| DomainError::storage(format!("Failed to read entity: {e}")))?;
                entity.aliases = Self::aliases_for(&conn, &entity.id)?;
                Ok(Some(entity))
            }
            None => Ok(None),
        }
    }

    async fn find_entity_by_alias(&self, alias: &str) -> Result<Option<Entity>, DomainError> {
        let needle = alias.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ENTITY_COLUMNS} FROM entities \
                 WHERE lower(canonical_name) = ?1 \
                    OR id IN (SELECT entity_id FROM entity_aliases WHERE lower(alias) = ?2) \
                 LIMIT 1"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare alias lookup: {e}")))?;
        let mut rows = stmt
            .query_map(params![needle, needle], Self::entity_base_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to query alias lookup: {e}")))?;
        match rows.next() {
            Some(row) => {
                let mut entity =
                    row.map_err(|e| DomainError::storage(format!("Failed to read entity: {e}")))?;
                entity.aliases = Self::aliases_for(&conn, &entity.id)?;
                Ok(Some(entity))
            }
            None => Ok(None),
        }
    }

    async fn list_entities(&self) -> Result<Vec<Entity>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ENTITY_COLUMNS} FROM entities ORDER BY updated_at DESC, id"
            ))
            .map_err(|e| DomainError::storage(format!("Failed to prepare list_entities: {e}")))?;
        let rows = stmt
            .query_map([], Self::entity_base_from_row)
            .map_err(|e| DomainError::storage(format!("Failed to list entities: {e}")))?;
        let mut entities = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read entity row: {e}")))?;
        for entity in &mut entities {
            entity.aliases = Self::aliases_for(&conn, &entity.id)?;
        }
        Ok(entities)
    }

    async fn search_entities_semantic(
        &self,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<(Entity, f32)>, DomainError> {
        let literal = self.vector_literal(vector)?;
        let cols = ENTITY_COLUMNS
            .split(", ")
            .map(|c| format!("e.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols}, 1.0 - array_cosine_distance(v.vector, {literal}) AS score \
             FROM entities e JOIN entity_vectors v ON v.entity_id = e.id \
             ORDER BY score DESC LIMIT {limit}"
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare entity search: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let entity = Self::entity_base_from_row(row)?;
                let score: f32 = row.get(ENTITY_COLUMNS.split(", ").count())?;
                Ok((entity, score))
            })
            .map_err(|e| DomainError::storage(format!("Entity semantic search failed: {e}")))?;
        let mut results = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read entity search row: {e}")))?;
        for (entity, _) in &mut results {
            entity.aliases = Self::aliases_for(&conn, &entity.id)?;
        }
        Ok(results)
    }

    async fn search_claims_semantic(
        &self,
        vector: &[f32],
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(Claim, f32)>, DomainError> {
        let literal = self.vector_literal(vector)?;
        let mut conditions = vec!["c.status = 'active'".to_string()];
        if let Some(p) = project {
            conditions.push(format!(
                "(c.project IS NULL OR c.project = '{}')",
                sql_quote(p)
            ));
        }
        let sql = format!(
            "SELECT {cols}, 1.0 - array_cosine_distance(v.vector, {literal}) AS score \
             FROM claims c JOIN claim_vectors v ON v.claim_id = c.id \
             WHERE {where_} \
             ORDER BY score DESC LIMIT {limit}",
            cols = Self::prefixed_claim_columns("c"),
            where_ = conditions.join(" AND "),
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare claim search: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let claim = Self::claim_from_row(row)?;
                let score: f32 = row.get(CLAIM_COLUMNS.split(", ").count())?;
                Ok((claim, score))
            })
            .map_err(|e| DomainError::storage(format!("Claim semantic search failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read claim search row: {e}")))
    }

    async fn search_claims_keyword(
        &self,
        query: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(Claim, f32)>, DomainError> {
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
        let match_cases: Vec<String> = terms
            .iter()
            .map(|t| {
                let e = escape(t);
                format!("(CASE WHEN lower(statement) LIKE '%{e}%' ESCAPE '\\' THEN 1 ELSE 0 END)")
            })
            .collect();
        let score_expr = format!("({}) / {}.0", match_cases.join(" + "), terms.len());
        let mut where_clause = String::from("status = 'active'");
        if let Some(p) = project {
            where_clause.push_str(&format!(
                " AND (project IS NULL OR project = '{}')",
                sql_quote(p)
            ));
        }
        let sql = format!(
            "SELECT {CLAIM_COLUMNS}, {score_expr} AS score FROM claims \
             WHERE {where_clause} AND {score_expr} > 0 \
             ORDER BY score DESC, recorded_at DESC LIMIT {limit}"
        );
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&sql).map_err(|e| {
            DomainError::storage(format!("Failed to prepare claim keyword search: {e}"))
        })?;
        let rows = stmt
            .query_map([], |row| {
                let claim = Self::claim_from_row(row)?;
                let score: f64 = row.get(CLAIM_COLUMNS.split(", ").count())?;
                Ok((claim, score as f32))
            })
            .map_err(|e| DomainError::storage(format!("Claim keyword search failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read claim keyword row: {e}")))
    }

    async fn stats(&self) -> Result<ClaimStoreStats, DomainError> {
        let conn = self.conn.lock().await;
        let total_claims: i64 = conn
            .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
            .map_err(|e| DomainError::storage(format!("Failed to count claims: {e}")))?;
        let total_entities: i64 = conn
            .query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))
            .map_err(|e| DomainError::storage(format!("Failed to count entities: {e}")))?;
        let total_edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM claim_edges", [], |row| row.get(0))
            .map_err(|e| DomainError::storage(format!("Failed to count edges: {e}")))?;

        let mut stmt = conn
            .prepare("SELECT status, COUNT(*) FROM claims GROUP BY status ORDER BY status")
            .map_err(|e| DomainError::storage(format!("Failed to prepare status counts: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .map_err(|e| DomainError::storage(format!("Failed to count by status: {e}")))?;
        let claims_by_status = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::storage(format!("Failed to read status count row: {e}")))?;

        Ok(ClaimStoreStats {
            total_claims: total_claims as u64,
            claims_by_status,
            total_entities: total_entities as u64,
            total_edges: total_edges as u64,
        })
    }
}

/// Escape single quotes for inlining a string into SQL (used for the `project`
/// filter, which is not a bound parameter because it sits inside a dynamically
/// built clause).
fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}
