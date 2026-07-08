use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::ChannelEndpointRepository;
use crate::domain::{ChannelEndpoint, ChannelRole, DomainError, EndpointSource, Protocol};

pub struct DuckdbChannelEndpointRepository {
    conn: Arc<Mutex<Connection>>,
}

impl DuckdbChannelEndpointRepository {
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
    /// Use this when the connection is read-only (DDL is forbidden). Read
    /// methods treat a missing `channel_endpoints` table as an empty result so
    /// that read-only commands work against databases indexed before this
    /// table existed.
    pub fn with_connection_no_init(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    fn initialize_schema(conn: &Connection) -> Result<(), DomainError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS channel_endpoints (
                id TEXT PRIMARY KEY,
                repository_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                enclosing_symbol TEXT,
                line INTEGER NOT NULL,
                protocol TEXT NOT NULL,
                role TEXT NOT NULL,
                channel_raw TEXT NOT NULL,
                channel_normalized TEXT NOT NULL,
                host TEXT,
                method TEXT,
                library TEXT,
                env_var TEXT,
                confirmed BOOLEAN DEFAULT FALSE,
                is_pattern BOOLEAN DEFAULT FALSE,
                resolved BOOLEAN DEFAULT TRUE,
                confidence FLOAT NOT NULL,
                source TEXT NOT NULL
            );

            -- Index for the matching join (producers ↔ consumers per channel)
            CREATE INDEX IF NOT EXISTS idx_chan_norm
            ON channel_endpoints(protocol, channel_normalized);

            -- Index for repository-wide operations
            CREATE INDEX IF NOT EXISTS idx_chan_repo
            ON channel_endpoints(repository_id);

            -- Index for the incremental-indexing file lifecycle
            CREATE INDEX IF NOT EXISTS idx_chan_file
            ON channel_endpoints(file_path, repository_id);
            "#,
        )
        .map_err(|e| {
            DomainError::storage(format!(
                "Failed to initialize channel_endpoints schema: {}",
                e
            ))
        })?;

        debug!("DuckDB channel_endpoints table initialized");
        Ok(())
    }

    /// Returns `true` when the error indicates the `channel_endpoints` table
    /// does not exist yet (read-only connection against a database indexed
    /// before this feature).
    fn is_missing_table(err: &duckdb::Error) -> bool {
        let msg = err.to_string();
        msg.contains("channel_endpoints") && msg.contains("does not exist")
    }

    fn row_to_endpoint(row: &duckdb::Row<'_>) -> duckdb::Result<ChannelEndpoint> {
        let protocol_str: String = row.get(5)?;
        let role_str: String = row.get(6)?;
        let source_str: String = row.get(17)?;

        // A stored string that no longer maps onto a known variant means the
        // row is corrupt or was written by an incompatible schema. Fail the
        // conversion so the problem surfaces instead of silently relabeling the
        // endpoint (e.g. an MQTT topic reported as HTTP).
        let enum_error = |col: usize, kind: &str, value: &str| {
            duckdb::Error::FromSqlConversionFailure(
                col,
                duckdb::types::Type::Text,
                format!("invalid {kind}: {value}").into(),
            )
        };

        let protocol = Protocol::parse(&protocol_str)
            .ok_or_else(|| enum_error(5, "protocol", &protocol_str))?;
        let role = ChannelRole::parse(&role_str).ok_or_else(|| enum_error(6, "role", &role_str))?;
        let source = EndpointSource::parse(&source_str)
            .ok_or_else(|| enum_error(17, "source", &source_str))?;

        Ok(ChannelEndpoint::reconstitute(
            row.get::<_, String>(0)?,         // id
            row.get::<_, String>(1)?,         // repository_id
            row.get::<_, String>(2)?,         // file_path
            row.get::<_, Option<String>>(3)?, // enclosing_symbol
            row.get::<_, i32>(4)? as u32,     // line
            protocol,
            role,
            row.get::<_, String>(7)?,          // channel_raw
            row.get::<_, String>(8)?,          // channel_normalized
            row.get::<_, Option<String>>(9)?,  // host
            row.get::<_, Option<String>>(10)?, // method
            row.get::<_, Option<String>>(11)?, // library
            row.get::<_, Option<String>>(12)?, // env_var
            row.get::<_, bool>(13)?,           // confirmed
            row.get::<_, bool>(14)?,           // is_pattern
            row.get::<_, bool>(15)?,           // resolved
            row.get::<_, f32>(16)?,            // confidence
            source,
        ))
    }

    const COLS: &'static str = "id, repository_id, file_path, enclosing_symbol, line, \
                                protocol, role, channel_raw, channel_normalized, host, \
                                method, library, env_var, confirmed, is_pattern, resolved, \
                                confidence, source";

    async fn query_endpoints(
        &self,
        sql: &str,
        params: &[String],
    ) -> Result<Vec<ChannelEndpoint>, DomainError> {
        let conn = self.conn.lock().await;
        let params_refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|p| p as &dyn duckdb::ToSql).collect();
        let mut stmt = match conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(e) if Self::is_missing_table(&e) => return Ok(Vec::new()),
            Err(e) => {
                return Err(DomainError::storage(format!(
                    "Failed to prepare statement: {}",
                    e
                )))
            }
        };

        let rows = stmt
            .query_map(params_refs.as_slice(), Self::row_to_endpoint)
            .map_err(|e| DomainError::storage(format!("Failed to query endpoints: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            results
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }
        Ok(results)
    }
}

#[async_trait]
impl ChannelEndpointRepository for DuckdbChannelEndpointRepository {
    async fn save_batch(&self, endpoints: &[ChannelEndpoint]) -> Result<(), DomainError> {
        if endpoints.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        {
            let mut stmt = tx
                .prepare(
                    r#"INSERT INTO channel_endpoints (
                        id, repository_id, file_path, enclosing_symbol, line,
                        protocol, role, channel_raw, channel_normalized, host,
                        method, library, env_var, confirmed, is_pattern,
                        resolved, confidence, source
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT (id) DO UPDATE SET
                        repository_id = excluded.repository_id,
                        file_path = excluded.file_path,
                        enclosing_symbol = excluded.enclosing_symbol,
                        line = excluded.line,
                        protocol = excluded.protocol,
                        role = excluded.role,
                        channel_raw = excluded.channel_raw,
                        channel_normalized = excluded.channel_normalized,
                        host = excluded.host,
                        method = excluded.method,
                        library = excluded.library,
                        env_var = excluded.env_var,
                        confirmed = excluded.confirmed,
                        is_pattern = excluded.is_pattern,
                        resolved = excluded.resolved,
                        confidence = excluded.confidence,
                        source = excluded.source
                    "#,
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

            for endpoint in endpoints {
                stmt.execute(params![
                    endpoint.id(),
                    endpoint.repository_id(),
                    endpoint.file_path(),
                    endpoint.enclosing_symbol(),
                    endpoint.line() as i32,
                    endpoint.protocol().as_str(),
                    endpoint.role().as_str(),
                    endpoint.channel_raw(),
                    endpoint.channel_normalized(),
                    endpoint.host(),
                    endpoint.method(),
                    endpoint.library(),
                    endpoint.env_var(),
                    endpoint.is_confirmed(),
                    endpoint.is_pattern(),
                    endpoint.is_resolved(),
                    endpoint.confidence(),
                    endpoint.source().as_str(),
                ])
                .map_err(|e| {
                    DomainError::storage(format!("Failed to save channel endpoint: {}", e))
                })?;
            }
        }

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!("Saved {} channel endpoints to DuckDB", endpoints.len());
        Ok(())
    }

    async fn find_by_repository(
        &self,
        repository_id: &str,
    ) -> Result<Vec<ChannelEndpoint>, DomainError> {
        let sql = format!(
            "SELECT {} FROM channel_endpoints WHERE repository_id = ? \
             ORDER BY file_path, line",
            Self::COLS
        );
        self.query_endpoints(&sql, &[repository_id.to_string()])
            .await
    }

    async fn find_by_protocol(
        &self,
        protocol: Protocol,
    ) -> Result<Vec<ChannelEndpoint>, DomainError> {
        let sql = format!(
            "SELECT {} FROM channel_endpoints WHERE protocol = ? \
             ORDER BY repository_id, file_path, line",
            Self::COLS
        );
        self.query_endpoints(&sql, &[protocol.as_str().to_string()])
            .await
    }

    async fn find_all(&self) -> Result<Vec<ChannelEndpoint>, DomainError> {
        let sql = format!(
            "SELECT {} FROM channel_endpoints \
             ORDER BY repository_id, file_path, line",
            Self::COLS
        );
        self.query_endpoints(&sql, &[]).await
    }

    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError> {
        let conn = self.conn.lock().await;

        let count = conn
            .execute(
                "DELETE FROM channel_endpoints WHERE repository_id = ? AND file_path = ?",
                params![repository_id, file_path],
            )
            .map_err(|e| DomainError::storage(format!("Failed to delete endpoints: {}", e)))?;

        debug!(
            "Deleted {} channel endpoints for file {} in repository {}",
            count, file_path, repository_id
        );
        Ok(count as u64)
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM channel_endpoints WHERE repository_id = ?",
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete endpoints: {}", e)))?;

        debug!(
            "Deleted all channel endpoints for repository {}",
            repository_id
        );
        Ok(())
    }

    async fn delete_synthesized_by_repository(
        &self,
        repository_id: &str,
    ) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        let count = conn
            .execute(
                "DELETE FROM channel_endpoints \
                 WHERE repository_id = ? AND source = ?",
                params![repository_id, EndpointSource::Config.as_str()],
            )
            .map_err(|e| {
                DomainError::storage(format!("Failed to delete synthesized endpoints: {}", e))
            })?;

        debug!(
            "Deleted {} synthesized channel endpoints for repository {}",
            count, repository_id
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ChannelRole, EndpointSource, Protocol};

    async fn create_test_repo() -> DuckdbChannelEndpointRepository {
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        DuckdbChannelEndpointRepository::with_connection(conn)
            .await
            .unwrap()
    }

    fn endpoint(repo: &str, file: &str, line: u32, role: ChannelRole) -> ChannelEndpoint {
        ChannelEndpoint::new(
            repo.to_string(),
            file.to_string(),
            line,
            Protocol::Kafka,
            role,
            "orders.created".to_string(),
            "orders.created".to_string(),
            0.9,
            EndpointSource::TreeSitter,
        )
        .with_enclosing_symbol("checkout")
    }

    #[tokio::test]
    async fn test_save_and_find_roundtrip() {
        let repo = create_test_repo().await;

        let ep = endpoint("repo-a", "src/app.py", 12, ChannelRole::Producer)
            .with_host("orders-svc")
            .with_method("post")
            .as_pattern()
            .unresolved();
        repo.save_batch(std::slice::from_ref(&ep)).await.unwrap();

        let found = repo.find_by_repository("repo-a").await.unwrap();
        assert_eq!(found.len(), 1);
        let f = &found[0];
        assert_eq!(f.id(), ep.id());
        assert_eq!(f.enclosing_symbol(), Some("checkout"));
        assert_eq!(f.protocol(), Protocol::Kafka);
        assert_eq!(f.role(), ChannelRole::Producer);
        assert_eq!(f.channel_raw(), "orders.created");
        assert_eq!(f.host(), Some("orders-svc"));
        assert_eq!(f.method(), Some("POST"));
        assert!(f.is_pattern());
        assert!(!f.is_resolved());
        assert_eq!(f.source(), EndpointSource::TreeSitter);
    }

    #[tokio::test]
    async fn test_save_batch_is_idempotent() {
        let repo = create_test_repo().await;

        let ep = endpoint("repo-a", "src/app.py", 12, ChannelRole::Producer);
        repo.save_batch(std::slice::from_ref(&ep)).await.unwrap();
        repo.save_batch(std::slice::from_ref(&ep)).await.unwrap();

        let found = repo.find_by_repository("repo-a").await.unwrap();
        assert_eq!(found.len(), 1, "upsert by id must not duplicate");
    }

    #[tokio::test]
    async fn test_find_by_protocol_and_all() {
        let repo = create_test_repo().await;

        repo.save_batch(&[
            endpoint("repo-a", "src/a.py", 1, ChannelRole::Producer),
            endpoint("repo-b", "src/b.js", 2, ChannelRole::Consumer),
        ])
        .await
        .unwrap();

        assert_eq!(
            repo.find_by_protocol(Protocol::Kafka).await.unwrap().len(),
            2
        );
        assert_eq!(
            repo.find_by_protocol(Protocol::Http).await.unwrap().len(),
            0
        );
        assert_eq!(repo.find_all().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_delete_lifecycle() {
        let repo = create_test_repo().await;

        repo.save_batch(&[
            endpoint("repo-a", "src/a.py", 1, ChannelRole::Producer),
            endpoint("repo-a", "src/b.py", 2, ChannelRole::Consumer),
            endpoint("repo-b", "src/c.js", 3, ChannelRole::Consumer),
        ])
        .await
        .unwrap();

        let deleted = repo
            .delete_by_file_path("repo-a", "src/a.py")
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(repo.find_by_repository("repo-a").await.unwrap().len(), 1);

        repo.delete_by_repository("repo-a").await.unwrap();
        assert_eq!(repo.find_by_repository("repo-a").await.unwrap().len(), 0);
        assert_eq!(repo.find_by_repository("repo-b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_delete_synthesized_keeps_extracted() {
        let repo = create_test_repo().await;

        let extracted = endpoint("repo-a", "src/a.py", 1, ChannelRole::Producer);
        let synthesized = ChannelEndpoint::new(
            "repo-a".to_string(),
            "src/b.ts".to_string(),
            5,
            Protocol::Kafka,
            ChannelRole::Consumer,
            "Consumer".to_string(),
            "Consumer".to_string(),
            0.6,
            EndpointSource::Config,
        )
        .with_id("synth:repo-a:src/b.ts:5:consumer")
        .unresolved();
        repo.save_batch(&[extracted, synthesized]).await.unwrap();
        assert_eq!(repo.find_by_repository("repo-a").await.unwrap().len(), 2);

        repo.delete_synthesized_by_repository("repo-a")
            .await
            .unwrap();

        // Only the tree-sitter endpoint remains.
        let remaining = repo.find_by_repository("repo-a").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].source(), EndpointSource::TreeSitter);
    }

    #[tokio::test]
    async fn test_missing_table_reads_as_empty() {
        // no-init constructor over a fresh connection: table does not exist.
        let conn = Connection::open_in_memory().unwrap();
        let repo =
            DuckdbChannelEndpointRepository::with_connection_no_init(Arc::new(Mutex::new(conn)));

        assert!(repo.find_by_repository("repo-a").await.unwrap().is_empty());
        assert!(repo.find_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_corrupt_enum_value_surfaces_error() {
        let repo = create_test_repo().await;

        // Write a row directly with an unknown protocol string, bypassing the
        // domain constructors, to simulate corruption or a forward-incompatible
        // schema. Reading it back must fail rather than relabel it as `http`.
        {
            let conn = repo.conn.lock().await;
            conn.execute(
                "INSERT INTO channel_endpoints (id, repository_id, file_path, line, \
                 protocol, role, channel_raw, channel_normalized, confidence, source) \
                 VALUES ('bad', 'repo-a', 'src/a.py', 1, 'smtp', 'producer', \
                 't', 't', 0.9, 'tree_sitter')",
                params![],
            )
            .unwrap();
        }

        let err = repo.find_by_repository("repo-a").await.unwrap_err();
        assert!(
            err.to_string().contains("smtp"),
            "error should name the offending value, got: {err}"
        );
    }
}
