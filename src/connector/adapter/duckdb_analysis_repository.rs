use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection};
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
const KIND_EXECUTION_FEATURES: &str = "execution_features";

/// `clusters.level` value for file-level clusters.
const LEVEL_FILE: &str = "file";
/// `clusters.level` value for symbol-level communities.
const LEVEL_SYMBOL: &str = "symbol";

/// Row shape shared by [`Cluster`] and [`SymbolCommunity`] — the two types are
/// structurally identical, so one set of SQL paths serves both levels.
struct CommunityRow {
    id: String,
    name: String,
    dominant_language: String,
    size: usize,
    cohesion: f32,
    members: Vec<String>,
}

/// DuckDB persistence for derived call-graph analyses (Leiden clusters,
/// symbol communities, execution features).
///
/// Results are stored per `(repository_id, kind)` and replaced wholesale on
/// save; a row in `analysis_runs` marks a stored (possibly empty) result, so
/// "never computed" and "computed, nothing found" stay distinguishable.
pub struct DuckdbAnalysisRepository {
    conn: Arc<Mutex<Connection>>,
}

impl DuckdbAnalysisRepository {
    /// Create a new adapter using an existing shared connection.
    pub async fn with_connection(conn: Arc<Mutex<Connection>>) -> Result<Self, DomainError> {
        let conn_guard = conn.lock().await;
        Self::initialize_schema(&conn_guard)?;
        drop(conn_guard);

        Ok(Self { conn })
    }

    /// Create a new adapter from a shared connection without running schema
    /// initialization.
    ///
    /// Use this when the connection is read-only (DDL is forbidden). Loads
    /// detect a missing schema and report "nothing stored" instead of erroring,
    /// so read-only commands degrade to recomputing their analysis.
    pub fn with_connection_no_init(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
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
                name TEXT NOT NULL,
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
                depth INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_execution_feature_nodes_repo
            ON execution_feature_nodes(repository_id);
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
    /// the run, all in one transaction.
    async fn save_communities(
        &self,
        repository_id: &str,
        level: &str,
        kind: &str,
        rows: &[CommunityRow],
        total_nodes: usize,
        total_edges: usize,
    ) -> Result<(), DomainError> {
        let mut conn = self.conn.lock().await;
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
                     (id, repository_id, level, name, dominant_language, size, cohesion) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
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
                        row.name,
                        row.dominant_language,
                        row.size as i64,
                        row.cohesion,
                    ])
                    .map_err(|e| DomainError::storage(format!("Failed to save cluster: {}", e)))?;
                for member in &row.members {
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
                "SELECT id, name, dominant_language, size, cohesion FROM clusters \
                 WHERE repository_id = ? AND level = ? ORDER BY size DESC, name, id",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;
        let cluster_rows = cluster_stmt
            .query_map(params![repository_id, level], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f32>(4)?,
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query clusters: {}", e)))?;

        let mut rows = Vec::new();
        for row in cluster_rows {
            let (id, name, dominant_language, size, cohesion) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            let members = members_by_cluster.remove(&id).unwrap_or_default();
            rows.push(CommunityRow {
                id,
                name,
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
        let rows: Vec<CommunityRow> = graph
            .clusters
            .iter()
            .map(|c| CommunityRow {
                id: c.id.clone(),
                name: c.name.clone(),
                dominant_language: c.dominant_language.clone(),
                size: c.size,
                cohesion: c.cohesion,
                members: c.members.clone(),
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
                name: row.name,
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
        let rows: Vec<CommunityRow> = graph
            .communities
            .iter()
            .map(|c| CommunityRow {
                id: c.id.clone(),
                name: c.name.clone(),
                dominant_language: c.dominant_language.clone(),
                size: c.size,
                cohesion: c.cohesion,
                members: c.members.clone(),
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
                name: row.name,
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
        let mut conn = self.conn.lock().await;
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
                     (feature_id, repository_id, seq, symbol, file_path, line, depth) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
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
                "SELECT feature_id, symbol, file_path, line, depth \
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
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query feature nodes: {}", e)))?;

        let mut nodes_by_feature: HashMap<String, Vec<FeatureNode>> = HashMap::new();
        for row in node_rows {
            let (feature_id, symbol, file_path, line, depth) =
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

        Ok(Some(features))
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        if !Self::schema_exists(&conn)? {
            return Ok(());
        }

        for sql in [
            "DELETE FROM cluster_members WHERE repository_id = ?",
            "DELETE FROM clusters WHERE repository_id = ?",
            "DELETE FROM execution_feature_nodes WHERE repository_id = ?",
            "DELETE FROM execution_features WHERE repository_id = ?",
            "DELETE FROM analysis_runs WHERE repository_id = ?",
        ] {
            conn.execute(sql, params![repository_id]).map_err(|e| {
                DomainError::storage(format!("Failed to delete stored analyses: {}", e))
            })?;
        }

        debug!("Deleted stored analyses for repository {}", repository_id);
        Ok(())
    }
}
