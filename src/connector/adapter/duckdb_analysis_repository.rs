use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use duckdb::{params, AccessMode, Config, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::AnalysisRepository;
use crate::domain::{
    Cluster, ClusterGraph, DomainError, ExecutionFeature, FeatureNode, SymbolCommunity,
    SymbolCommunityGraph,
};

/// `analysis_runs.kind` value for the file-level Leiden cluster graph.
const KIND_FILE_CLUSTERS: &str = "file_clusters";
/// `analysis_runs.kind` value for the symbol-level Leiden community graph.
const KIND_SYMBOL_COMMUNITIES: &str = "symbol_communities";
/// `analysis_runs.kind` value for the execution-feature set.
// The suffix versions the cache: v1 runs used a single-repository BFS
// (cross-repo flows truncated at the first hop); v2 lacked per-node
// repository labels; v3 detected entry points repo-locally, so shared-library
// methods called only from sibling repos masqueraded as library entry points.
// Older kinds simply miss and recompute; their rows are cleared by the
// store's per-repository DELETE.
const KIND_EXECUTION_FEATURES: &str = "execution_features_v4";

/// `clusters.level` value for file-level clusters.
const LEVEL_FILE: &str = "file";
/// `clusters.level` value for symbol-level communities.
const LEVEL_SYMBOL: &str = "symbol";

/// Owned row shape shared by [`Cluster`] and [`SymbolCommunity`] — the two types
/// are structurally identical, so one set of SQL paths serves both levels. Used
/// on the load path, where the data must outlive the connection borrow.
struct CommunityRow {
    id: String,
    dominant_language: String,
    size: usize,
    cohesion: f32,
    members: Vec<String>,
}

/// Borrowed view of a community for the save path — lives only for the duration
/// of one write transaction, so it borrows the domain object's fields instead of
/// cloning them.
struct CommunityRowRef<'a> {
    id: &'a str,
    dominant_language: &'a str,
    size: usize,
    cohesion: f32,
    members: &'a [String],
}

/// Maximum number of retry attempts when the deferred write-back connection
/// fails to acquire the write lock because another process (an ongoing
/// `codesearch index`, or a concurrent analysis) holds it.
const WRITE_BACK_LOCK_RETRIES: u32 = 3;

/// Initial backoff for write-back lock-conflict retries. Doubles each attempt:
/// 200 ms → 400 ms → 800 ms (≈ 1.4 s total before giving up and skipping the
/// cache). Kept short: the query result is already in hand, so a failed cache
/// write only costs the next run its warm start, never correctness.
const WRITE_BACK_LOCK_RETRY_INITIAL_MS: u64 = 200;

/// Returns `true` when the error looks like a DuckDB file-lock conflict raised
/// by a concurrent writer process.
fn is_lock_conflict(err: &str) -> bool {
    err.contains("Could not set lock on file") || err.contains("Conflicting lock is held")
}

/// DuckDB persistence for derived call-graph analyses (Leiden clusters,
/// symbol communities, execution features).
///
/// Results are stored per `(repository_id, kind)` and replaced wholesale on
/// save; a row in `analysis_runs` marks a stored (possibly empty) result, so
/// "never computed" and "computed, nothing found" stay distinguishable.
///
/// # Read-only mode and deferred write-back
///
/// Read commands (`features`, `visualize`, …) open the database read-only so
/// they never hold the exclusive write lock and any number of them can run
/// concurrently. The shared `conn` is then read-only and cannot persist a
/// freshly computed analysis. Rather than forfeit caching, such a repository is
/// built with [`with_read_only_write_back`], recording the database path in
/// `write_path`. On a `save_*`, it opens a **short-lived writable connection**
/// against that path — after the read-only work is done and the result is
/// already in hand — writes the cache, and drops the connection. The read path
/// stays lock-free; only the brief flush touches the write lock, and it is
/// best-effort: a lock conflict is retried briefly, then skipped.
pub struct DuckdbAnalysisRepository {
    /// Shared connection used for all loads. Writable in normal mode; read-only
    /// when `write_path` is set.
    conn: Arc<Mutex<Connection>>,
    /// When set, `conn` is read-only and saves go through a short-lived writable
    /// connection opened against this database path (deferred write-back).
    write_path: Option<PathBuf>,
}

impl DuckdbAnalysisRepository {
    /// Create a new adapter using an existing (writable) shared connection.
    pub async fn with_connection(conn: Arc<Mutex<Connection>>) -> Result<Self, DomainError> {
        let conn_guard = conn.lock().await;
        Self::initialize_schema(&conn_guard)?;
        drop(conn_guard);

        Ok(Self {
            conn,
            write_path: None,
        })
    }

    /// Create a new adapter from a shared connection without running schema
    /// initialization.
    ///
    /// Use this when the connection is read-only (DDL is forbidden). Loads
    /// detect a missing schema and report "nothing stored" instead of erroring,
    /// so read-only commands degrade to recomputing their analysis. Saves are
    /// no-ops (no `write_path`); prefer [`with_read_only_write_back`] when the
    /// cache should still be filled.
    pub fn with_connection_no_init(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            write_path: None,
        }
    }

    /// Create a read-only adapter that still fills the cache via deferred
    /// write-back: loads use the shared read-only `conn`, while saves open a
    /// short-lived writable connection against `db_path` (see the type docs).
    pub fn with_read_only_write_back(conn: Arc<Mutex<Connection>>, db_path: &Path) -> Self {
        Self {
            conn,
            write_path: Some(db_path.to_path_buf()),
        }
    }

    /// Open a short-lived writable connection against `path` for deferred
    /// write-back, retrying briefly on a cross-process lock conflict.
    ///
    /// The extensions/HNSW settings the reader loads are unnecessary here — the
    /// analysis tables are plain relational tables — so this only ensures the
    /// analysis schema exists before returning.
    async fn open_write_back_conn(path: &Path) -> Result<Connection, DomainError> {
        let mut delay_ms = WRITE_BACK_LOCK_RETRY_INITIAL_MS;
        for attempt in 0..=WRITE_BACK_LOCK_RETRIES {
            let config = Config::default()
                .access_mode(AccessMode::ReadWrite)
                .map_err(|e| {
                    DomainError::storage(format!("Failed to configure write-back access: {}", e))
                })?;
            match Connection::open_with_flags(path, config) {
                Ok(conn) => {
                    Self::initialize_schema(&conn)?;
                    return Ok(conn);
                }
                Err(e) if attempt < WRITE_BACK_LOCK_RETRIES && is_lock_conflict(&e.to_string()) => {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms *= 2;
                }
                Err(e) => {
                    return Err(DomainError::storage(format!(
                        "Failed to open write-back connection: {}",
                        e
                    )))
                }
            }
        }
        unreachable!()
    }

    /// Run `op` against a writable connection, transparently choosing between
    /// the shared writable connection (normal mode) and a short-lived
    /// write-back connection (read-only mode).
    ///
    /// In write-back mode a lock conflict never surfaces as an error: the query
    /// result is already in hand, so a cache write that can't get the lock is
    /// logged at debug and skipped.
    async fn with_write_conn<F>(&self, op: F) -> Result<(), DomainError>
    where
        F: FnOnce(&mut Connection) -> Result<(), DomainError>,
    {
        match &self.write_path {
            None => {
                let mut conn = self.conn.lock().await;
                op(&mut conn)
            }
            Some(path) => {
                let mut conn = Self::open_write_back_conn(path).await?;
                op(&mut conn)
            }
        }
    }

    fn initialize_schema(conn: &Connection) -> Result<(), DomainError> {
        conn.execute_batch(
            r#"
            -- One row per stored analysis: marks the result as present (even
            -- when empty) and records the size of the underlying graph.
            CREATE TABLE IF NOT EXISTS analysis_runs (
                repository_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                total_nodes BIGINT NOT NULL,
                total_edges BIGINT NOT NULL,
                computed_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                PRIMARY KEY (repository_id, kind)
            );

            -- Leiden results at both levels: level='file' rows are file
            -- clusters, level='symbol' rows are symbol communities.
            CREATE TABLE IF NOT EXISTS clusters (
                id TEXT PRIMARY KEY,
                repository_id TEXT NOT NULL,
                level TEXT NOT NULL,
                dominant_language TEXT NOT NULL,
                size BIGINT NOT NULL,
                cohesion FLOAT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_clusters_repo_level
            ON clusters(repository_id, level);

            -- Member = file path (level='file') or symbol FQN (level='symbol').
            CREATE TABLE IF NOT EXISTS cluster_members (
                cluster_id TEXT NOT NULL,
                repository_id TEXT NOT NULL,
                level TEXT NOT NULL,
                member TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_cluster_members_repo_level
            ON cluster_members(repository_id, level);

            CREATE TABLE IF NOT EXISTS execution_features (
                id TEXT PRIMARY KEY,
                repository_id TEXT NOT NULL,
                name TEXT NOT NULL,
                entry_point TEXT NOT NULL,
                depth BIGINT NOT NULL,
                file_count BIGINT NOT NULL,
                reach BIGINT NOT NULL,
                criticality FLOAT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_execution_features_repo
            ON execution_features(repository_id);

            -- BFS call-chain nodes of a feature, ordered by seq.
            CREATE TABLE IF NOT EXISTS execution_feature_nodes (
                feature_id TEXT NOT NULL,
                repository_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                symbol TEXT NOT NULL,
                file_path TEXT NOT NULL,
                line INTEGER NOT NULL,
                depth INTEGER NOT NULL,
                caller TEXT,
                callee_count INTEGER,
                node_repository TEXT
            );

            -- Migration for databases created before `caller`/`callee_count`/
            -- `node_repository` existed. Old stored runs are versioned out by
            -- the analysis KIND (see KIND_EXECUTION_FEATURES), so they
            -- recompute rather than serve incomplete data.
            ALTER TABLE execution_feature_nodes ADD COLUMN IF NOT EXISTS caller TEXT;
            ALTER TABLE execution_feature_nodes ADD COLUMN IF NOT EXISTS callee_count INTEGER;
            ALTER TABLE execution_feature_nodes ADD COLUMN IF NOT EXISTS node_repository TEXT;

            CREATE INDEX IF NOT EXISTS idx_execution_feature_nodes_repo
            ON execution_feature_nodes(repository_id);

            -- Cache of LLM-generated display names, keyed on the stable,
            -- content-addressed community id. Intentionally NOT scoped to a
            -- repository and NOT wiped by delete_by_repository: the id is a pure
            -- function of a community's membership, so an unchanged community
            -- keeps its name across re-index for free, while a changed one gets a
            -- new id (a cache miss) rather than a stale name.
            CREATE TABLE IF NOT EXISTS community_names (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                generated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
            );

            -- Cache of LLM call-flow explanations, keyed by (repository, symbol,
            -- model). Scoped to the repository and wiped by delete_by_repository
            -- on re-index, so a cached explanation only ever describes the
            -- current index. The model is part of the key so switching models
            -- serves (or computes) the right one rather than a stale answer.
            CREATE TABLE IF NOT EXISTS explanations (
                repository_id TEXT NOT NULL,
                symbol TEXT NOT NULL,
                model TEXT NOT NULL,
                explanation TEXT NOT NULL,
                computed_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                PRIMARY KEY (repository_id, symbol, model)
            );

            CREATE INDEX IF NOT EXISTS idx_explanations_repo
            ON explanations(repository_id);
            "#,
        )
        .map_err(|e| {
            DomainError::storage(format!("Failed to initialize analysis schema: {}", e))
        })?;

        debug!("DuckDB analysis tables initialized");
        Ok(())
    }

    /// Whether the analysis schema exists on this connection. False on a
    /// read-only connection opened before any writable process created the
    /// tables — loads then report "nothing stored" instead of erroring.
    fn schema_exists(conn: &Connection) -> Result<bool, DomainError> {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'analysis_runs'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| DomainError::storage(format!("Failed to check analysis schema: {}", e)))?;
        Ok(count > 0)
    }

    /// Load the `(total_nodes, total_edges)` of a stored run, or `None` when
    /// nothing has been stored for `(repository_id, kind)`.
    fn load_run(
        conn: &Connection,
        repository_id: &str,
        kind: &str,
    ) -> Result<Option<(usize, usize)>, DomainError> {
        let mut stmt = conn
            .prepare(
                "SELECT total_nodes, total_edges FROM analysis_runs \
                 WHERE repository_id = ? AND kind = ?",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let mut rows = stmt
            .query_map(params![repository_id, kind], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query analysis run: {}", e)))?;

        match rows.next() {
            Some(row) => {
                let (nodes, edges) =
                    row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
                Ok(Some((nodes as usize, edges as usize)))
            }
            None => Ok(None),
        }
    }

    /// Replace the stored communities for `(repository_id, level)` and record
    /// the run, all in one transaction, on a caller-provided writable
    /// connection.
    fn write_communities_tx(
        conn: &mut Connection,
        repository_id: &str,
        level: &str,
        kind: &str,
        rows: &[CommunityRowRef<'_>],
        total_nodes: usize,
        total_edges: usize,
    ) -> Result<(), DomainError> {
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        tx.execute(
            "DELETE FROM cluster_members WHERE repository_id = ? AND level = ?",
            params![repository_id, level],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear cluster members: {}", e)))?;
        tx.execute(
            "DELETE FROM clusters WHERE repository_id = ? AND level = ?",
            params![repository_id, level],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear clusters: {}", e)))?;
        tx.execute(
            "DELETE FROM analysis_runs WHERE repository_id = ? AND kind = ?",
            params![repository_id, kind],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear analysis run: {}", e)))?;

        {
            let mut cluster_stmt = tx
                .prepare(
                    "INSERT INTO clusters \
                     (id, repository_id, level, dominant_language, size, cohesion) \
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
            let mut member_stmt = tx
                .prepare(
                    "INSERT INTO cluster_members (cluster_id, repository_id, level, member) \
                     VALUES (?, ?, ?, ?)",
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

            for row in rows {
                cluster_stmt
                    .execute(params![
                        row.id,
                        repository_id,
                        level,
                        row.dominant_language,
                        row.size as i64,
                        row.cohesion,
                    ])
                    .map_err(|e| DomainError::storage(format!("Failed to save cluster: {}", e)))?;
                for member in row.members {
                    member_stmt
                        .execute(params![row.id, repository_id, level, member])
                        .map_err(|e| {
                            DomainError::storage(format!("Failed to save cluster member: {}", e))
                        })?;
                }
            }
        }

        tx.execute(
            "INSERT INTO analysis_runs (repository_id, kind, total_nodes, total_edges) \
             VALUES (?, ?, ?, ?)",
            params![repository_id, kind, total_nodes as i64, total_edges as i64],
        )
        .map_err(|e| DomainError::storage(format!("Failed to record analysis run: {}", e)))?;

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!(
            "Saved {} {} communities for repository {}",
            rows.len(),
            level,
            repository_id
        );
        Ok(())
    }

    /// Replace the stored communities for `(repository_id, level)`, choosing a
    /// writable connection via [`with_write_conn`] so the same path serves both
    /// normal and read-only (deferred write-back) modes.
    async fn save_communities(
        &self,
        repository_id: &str,
        level: &str,
        kind: &str,
        rows: &[CommunityRowRef<'_>],
        total_nodes: usize,
        total_edges: usize,
    ) -> Result<(), DomainError> {
        self.with_write_conn(|conn| {
            Self::write_communities_tx(
                conn,
                repository_id,
                level,
                kind,
                rows,
                total_nodes,
                total_edges,
            )
        })
        .await
    }

    /// Replace the stored execution features for `repository_id` and record the
    /// run, all in one transaction, on a caller-provided writable connection.
    fn write_execution_features_tx(
        conn: &mut Connection,
        repository_id: &str,
        features: &[ExecutionFeature],
    ) -> Result<(), DomainError> {
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        tx.execute(
            "DELETE FROM execution_feature_nodes WHERE repository_id = ?",
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear feature nodes: {}", e)))?;
        tx.execute(
            "DELETE FROM execution_features WHERE repository_id = ?",
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear features: {}", e)))?;
        tx.execute(
            "DELETE FROM analysis_runs WHERE repository_id = ? AND kind = ?",
            params![repository_id, KIND_EXECUTION_FEATURES],
        )
        .map_err(|e| DomainError::storage(format!("Failed to clear analysis run: {}", e)))?;

        {
            let mut feature_stmt = tx
                .prepare(
                    "INSERT INTO execution_features \
                     (id, repository_id, name, entry_point, depth, file_count, reach, criticality) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
            let mut node_stmt = tx
                .prepare(
                    "INSERT INTO execution_feature_nodes \
                     (feature_id, repository_id, seq, symbol, file_path, line, depth, caller, callee_count, node_repository) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

            for feature in features {
                feature_stmt
                    .execute(params![
                        feature.id,
                        repository_id,
                        feature.name,
                        feature.entry_point,
                        feature.depth as i64,
                        feature.file_count as i64,
                        feature.reach as i64,
                        feature.criticality,
                    ])
                    .map_err(|e| DomainError::storage(format!("Failed to save feature: {}", e)))?;
                for (seq, node) in feature.path.iter().enumerate() {
                    node_stmt
                        .execute(params![
                            feature.id,
                            repository_id,
                            seq as i32,
                            node.symbol,
                            node.file_path,
                            node.line as i32,
                            node.depth as i32,
                            node.caller,
                            node.callee_count as i32,
                            node.repository_name,
                        ])
                        .map_err(|e| {
                            DomainError::storage(format!("Failed to save feature node: {}", e))
                        })?;
                }
            }
        }

        tx.execute(
            "INSERT INTO analysis_runs (repository_id, kind, total_nodes, total_edges) \
             VALUES (?, ?, ?, 0)",
            params![
                repository_id,
                KIND_EXECUTION_FEATURES,
                features.len() as i64
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to record analysis run: {}", e)))?;

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!(
            "Saved {} execution features for repository {}",
            features.len(),
            repository_id
        );
        Ok(())
    }

    /// Load the stored communities for `(repository_id, level)` together with
    /// the recorded graph totals, or `None` when no run has been stored.
    async fn load_communities(
        &self,
        repository_id: &str,
        level: &str,
        kind: &str,
    ) -> Result<Option<(Vec<CommunityRow>, usize, usize)>, DomainError> {
        let conn = self.conn.lock().await;
        if !Self::schema_exists(&conn)? {
            return Ok(None);
        }
        let Some((total_nodes, total_edges)) = Self::load_run(&conn, repository_id, kind)? else {
            return Ok(None);
        };

        // Members first, grouped by cluster id (sorted so each member list
        // comes back in the alphabetical order the domain types promise).
        let mut member_stmt = conn
            .prepare(
                "SELECT cluster_id, member FROM cluster_members \
                 WHERE repository_id = ? AND level = ? ORDER BY cluster_id, member",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
        let member_rows = member_stmt
            .query_map(params![repository_id, level], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query cluster members: {}", e)))?;

        let mut members_by_cluster: HashMap<String, Vec<String>> = HashMap::new();
        for row in member_rows {
            let (cluster_id, member) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            members_by_cluster
                .entry(cluster_id)
                .or_default()
                .push(member);
        }

        // Largest first, then name — the order the detection use cases emit.
        let mut cluster_stmt = conn
            .prepare(
                "SELECT id, dominant_language, size, cohesion FROM clusters \
                 WHERE repository_id = ? AND level = ? ORDER BY size DESC, id",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
        let cluster_rows = cluster_stmt
            .query_map(params![repository_id, level], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, f32>(3)?,
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query clusters: {}", e)))?;

        let mut rows = Vec::new();
        for row in cluster_rows {
            let (id, dominant_language, size, cohesion) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            let members = members_by_cluster.remove(&id).unwrap_or_default();
            rows.push(CommunityRow {
                id,
                dominant_language,
                size: size as usize,
                cohesion,
                members,
            });
        }

        Ok(Some((rows, total_nodes, total_edges)))
    }
}

#[async_trait]
impl AnalysisRepository for DuckdbAnalysisRepository {
    async fn save_cluster_graph(&self, graph: &ClusterGraph) -> Result<(), DomainError> {
        let rows: Vec<CommunityRowRef<'_>> = graph
            .clusters
            .iter()
            .map(|c| CommunityRowRef {
                id: &c.id,
                dominant_language: &c.dominant_language,
                size: c.size,
                cohesion: c.cohesion,
                members: &c.members,
            })
            .collect();
        self.save_communities(
            &graph.repository_id,
            LEVEL_FILE,
            KIND_FILE_CLUSTERS,
            &rows,
            graph.total_files,
            graph.total_edges,
        )
        .await
    }

    async fn load_cluster_graph(
        &self,
        repository_id: &str,
    ) -> Result<Option<ClusterGraph>, DomainError> {
        let Some((rows, total_files, total_edges)) = self
            .load_communities(repository_id, LEVEL_FILE, KIND_FILE_CLUSTERS)
            .await?
        else {
            return Ok(None);
        };

        let clusters = rows
            .into_iter()
            .map(|row| Cluster {
                id: row.id,
                // Filled from the persistent name cache at render time; the
                // cluster graph itself is rewritten on every recompute, so the
                // LLM name is kept in a separate table keyed on the stable id.
                display_name: None,
                repository_id: repository_id.to_string(),
                dominant_language: row.dominant_language,
                size: row.size,
                cohesion: row.cohesion,
                members: row.members,
            })
            .collect();

        Ok(Some(ClusterGraph {
            clusters,
            repository_id: repository_id.to_string(),
            total_files,
            total_edges,
        }))
    }

    async fn save_symbol_community_graph(
        &self,
        graph: &SymbolCommunityGraph,
    ) -> Result<(), DomainError> {
        let rows: Vec<CommunityRowRef<'_>> = graph
            .communities
            .iter()
            .map(|c| CommunityRowRef {
                id: &c.id,
                dominant_language: &c.dominant_language,
                size: c.size,
                cohesion: c.cohesion,
                members: &c.members,
            })
            .collect();
        self.save_communities(
            &graph.repository_id,
            LEVEL_SYMBOL,
            KIND_SYMBOL_COMMUNITIES,
            &rows,
            graph.total_symbols,
            graph.total_edges,
        )
        .await
    }

    async fn load_symbol_community_graph(
        &self,
        repository_id: &str,
    ) -> Result<Option<SymbolCommunityGraph>, DomainError> {
        let Some((rows, total_symbols, total_edges)) = self
            .load_communities(repository_id, LEVEL_SYMBOL, KIND_SYMBOL_COMMUNITIES)
            .await?
        else {
            return Ok(None);
        };

        let communities = rows
            .into_iter()
            .map(|row| SymbolCommunity {
                id: row.id,
                // Filled from the persistent name cache at render time (see the
                // file-cluster loader for why it is kept out of this table).
                display_name: None,
                repository_id: repository_id.to_string(),
                dominant_language: row.dominant_language,
                size: row.size,
                cohesion: row.cohesion,
                members: row.members,
            })
            .collect();

        Ok(Some(SymbolCommunityGraph {
            communities,
            repository_id: repository_id.to_string(),
            total_symbols,
            total_edges,
        }))
    }

    async fn save_execution_features(
        &self,
        repository_id: &str,
        features: &[ExecutionFeature],
    ) -> Result<(), DomainError> {
        self.with_write_conn(|conn| {
            Self::write_execution_features_tx(conn, repository_id, features)
        })
        .await
    }

    async fn load_execution_features(
        &self,
        repository_id: &str,
    ) -> Result<Option<Vec<ExecutionFeature>>, DomainError> {
        let conn = self.conn.lock().await;
        if !Self::schema_exists(&conn)? {
            return Ok(None);
        }
        if Self::load_run(&conn, repository_id, KIND_EXECUTION_FEATURES)?.is_none() {
            return Ok(None);
        }

        let mut node_stmt = conn
            .prepare(
                "SELECT feature_id, symbol, file_path, line, depth, caller, callee_count, node_repository \
                 FROM execution_feature_nodes WHERE repository_id = ? ORDER BY feature_id, seq",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
        let node_rows = node_stmt
            .query_map(params![repository_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i32>(3)?,
                    row.get::<_, i32>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i32>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query feature nodes: {}", e)))?;

        let mut nodes_by_feature: HashMap<String, Vec<FeatureNode>> = HashMap::new();
        for row in node_rows {
            let (feature_id, symbol, file_path, line, depth, caller, callee_count, repository_name) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            nodes_by_feature
                .entry(feature_id)
                .or_default()
                .push(FeatureNode {
                    symbol,
                    file_path,
                    line: line as u32,
                    depth: depth as usize,
                    repository_id: repository_id.to_string(),
                    caller,
                    callee_count: callee_count.unwrap_or(0) as usize,
                    repository_name,
                });
        }

        let mut feature_stmt = conn
            .prepare(
                "SELECT id, name, entry_point, depth, file_count, reach, criticality \
                 FROM execution_features WHERE repository_id = ? \
                 ORDER BY criticality DESC, id",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
        let feature_rows = feature_stmt
            .query_map(params![repository_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, f32>(6)?,
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query features: {}", e)))?;

        let mut features = Vec::new();
        for row in feature_rows {
            let (id, name, entry_point, depth, file_count, reach, criticality) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            let path = nodes_by_feature.remove(&id).unwrap_or_default();
            features.push(ExecutionFeature {
                id,
                name,
                entry_point,
                repository_id: repository_id.to_string(),
                path,
                depth: depth as usize,
                file_count: file_count as usize,
                reach: reach as usize,
                criticality,
            });
        }

        // A run stored before `caller` existed serves flat paths that can't
        // fold into a call tree — report a cache miss so the caller recomputes
        // (and re-stores) with parentage. New runs always set `caller` on every
        // non-entry node, so all-None with a multi-node path means "legacy".
        let legacy = features
            .iter()
            .any(|f| f.path.len() > 1 && f.path.iter().all(|n| n.caller.is_none()));
        if legacy {
            return Ok(None);
        }

        Ok(Some(features))
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        // Short-circuit on the shared read connection so write-back mode never
        // opens a writable connection just to find nothing to delete.
        {
            let conn = self.conn.lock().await;
            if !Self::schema_exists(&conn)? {
                return Ok(());
            }
        }

        self.with_write_conn(|conn| {
            // All five deletes commit together: a partial delete would leave
            // orphaned members/nodes pointing at a removed run.
            let tx = conn
                .transaction()
                .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;
            for sql in [
                "DELETE FROM cluster_members WHERE repository_id = ?",
                "DELETE FROM clusters WHERE repository_id = ?",
                "DELETE FROM execution_feature_nodes WHERE repository_id = ?",
                "DELETE FROM execution_features WHERE repository_id = ?",
                "DELETE FROM analysis_runs WHERE repository_id = ?",
                // Cached explanations derive from the call graph too, so a
                // re-index must drop them — this is the sole invalidation path.
                "DELETE FROM explanations WHERE repository_id = ?",
            ] {
                tx.execute(sql, params![repository_id]).map_err(|e| {
                    DomainError::storage(format!("Failed to delete stored analyses: {}", e))
                })?;
            }
            tx.commit()
                .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;
            debug!("Deleted stored analyses for repository {}", repository_id);
            Ok(())
        })
        .await
    }

    async fn get_community_names(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, String>, DomainError> {
        let mut out = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let conn = self.conn.lock().await;
        if !Self::schema_exists(&conn)? {
            return Ok(out);
        }

        // One IN (...) query with a placeholder per id keeps this a single round
        // trip regardless of community count.
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id, name FROM community_names WHERE id IN ({placeholders})");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
        let params = duckdb::params_from_iter(ids.iter());
        let rows = stmt
            .query_map(params, |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query community names: {}", e)))?;
        for row in rows {
            let (id, name) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            out.insert(id, name);
        }
        Ok(out)
    }

    async fn save_community_names(&self, names: &[(String, String)]) -> Result<(), DomainError> {
        if names.is_empty() {
            return Ok(());
        }
        let names = names.to_vec();
        self.with_write_conn(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;
            for (id, name) in &names {
                // `generated_at` is left to its column DEFAULT (current_timestamp)
                // on insert and untouched on conflict: DuckDB parses a bare
                // `current_timestamp` in a DO UPDATE SET as a column reference and
                // errors, and the timestamp is only informational anyway.
                tx.execute(
                    "INSERT INTO community_names (id, name) VALUES (?, ?) \
                     ON CONFLICT (id) DO UPDATE SET name = excluded.name",
                    params![id, name],
                )
                .map_err(|e| {
                    DomainError::storage(format!("Failed to save community name: {}", e))
                })?;
            }
            tx.commit()
                .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;
            Ok(())
        })
        .await
    }

    async fn get_cached_explanation(
        &self,
        repository_id: &str,
        symbol: &str,
        model: &str,
    ) -> Result<Option<String>, DomainError> {
        let conn = self.conn.lock().await;
        if !Self::schema_exists(&conn)? {
            return Ok(None);
        }
        let mut stmt = conn
            .prepare(
                "SELECT explanation FROM explanations \
                 WHERE repository_id = ? AND symbol = ? AND model = ?",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
        let mut rows = stmt
            .query_map(params![repository_id, symbol, model], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| {
                DomainError::storage(format!("Failed to query cached explanation: {}", e))
            })?;
        match rows.next() {
            Some(row) => Ok(Some(row.map_err(|e| {
                DomainError::storage(format!("Failed to read explanation row: {}", e))
            })?)),
            None => Ok(None),
        }
    }

    async fn save_explanation(
        &self,
        repository_id: &str,
        symbol: &str,
        model: &str,
        explanation: &str,
    ) -> Result<(), DomainError> {
        let (repository_id, symbol, model, explanation) = (
            repository_id.to_string(),
            symbol.to_string(),
            model.to_string(),
            explanation.to_string(),
        );
        self.with_write_conn(move |conn| {
            // `computed_at` is left to its column DEFAULT on insert and untouched
            // on conflict (same DuckDB caveat as community_names). A Regenerate
            // overwrites the previous explanation for this exact key.
            conn.execute(
                "INSERT INTO explanations (repository_id, symbol, model, explanation) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT (repository_id, symbol, model) \
                 DO UPDATE SET explanation = excluded.explanation",
                params![repository_id, symbol, model, explanation],
            )
            .map_err(|e| DomainError::storage(format!("Failed to save explanation: {}", e)))?;
            Ok(())
        })
        .await
    }
}
