use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::{ChannelEndpointRepository, ChannelStats};
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
        let source_str: String = row.get(13)?;

        Ok(ChannelEndpoint::reconstitute(
            row.get::<_, String>(0)?,         // id
            row.get::<_, String>(1)?,         // repository_id
            row.get::<_, String>(2)?,         // file_path
            row.get::<_, Option<String>>(3)?, // enclosing_symbol
            row.get::<_, i32>(4)? as u32,     // line
            Protocol::parse(&protocol_str).unwrap_or(Protocol::Http),
            ChannelRole::parse(&role_str).unwrap_or(ChannelRole::Producer),
            row.get::<_, String>(7)?,         // channel_raw
            row.get::<_, String>(8)?,         // channel_normalized
            row.get::<_, Option<String>>(9)?, // host
            row.get::<_, bool>(10)?,          // is_pattern
            row.get::<_, bool>(11)?,          // resolved
            row.get::<_, f32>(12)?,           // confidence
            EndpointSource::parse(&source_str).unwrap_or(EndpointSource::TreeSitter),
        ))
    }

    const COLS: &'static str = "id, repository_id, file_path, enclosing_symbol, line, \
                                protocol, role, channel_raw, channel_normalized, host, \
                                is_pattern, resolved, confidence, source";

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
                        is_pattern, resolved, confidence, source
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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

    async fn get_stats(&self, repository_id: &str) -> Result<ChannelStats, DomainError> {
        let conn = self.conn.lock().await;

        let totals = conn.query_row(
            "SELECT COUNT(*), \
                    COUNT(*) FILTER (WHERE role = 'producer'), \
                    COUNT(*) FILTER (WHERE role = 'consumer'), \
                    COUNT(*) FILTER (WHERE NOT resolved) \
             FROM channel_endpoints WHERE repository_id = ?",
            params![repository_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        );
        let (total, producers, consumers, unresolved) = match totals {
            Ok(t) => t,
            Err(ref e) if Self::is_missing_table(e) => return Ok(ChannelStats::default()),
            Err(e) => {
                return Err(DomainError::storage(format!(
                    "Failed to count endpoints: {}",
                    e
                )))
            }
        };

        let mut stmt = conn
            .prepare(
                "SELECT protocol, COUNT(*) FROM channel_endpoints \
                 WHERE repository_id = ? GROUP BY protocol ORDER BY COUNT(*) DESC",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let rows = stmt
            .query_map(params![repository_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query by protocol: {}", e)))?;

        let mut by_protocol = Vec::new();
        for row in rows {
            let (protocol, count) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            by_protocol.push((protocol, count as u64));
        }

        Ok(ChannelStats {
            total_endpoints: total as u64,
            producers: producers as u64,
            consumers: consumers as u64,
            unresolved: unresolved as u64,
            by_protocol,
        })
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
    async fn test_get_stats() {
        let repo = create_test_repo().await;

        repo.save_batch(&[
            endpoint("repo-a", "src/a.py", 1, ChannelRole::Producer),
            endpoint("repo-a", "src/b.py", 2, ChannelRole::Consumer),
            endpoint("repo-a", "src/c.py", 3, ChannelRole::Consumer).unresolved(),
        ])
        .await
        .unwrap();

        let stats = repo.get_stats("repo-a").await.unwrap();
        assert_eq!(stats.total_endpoints, 3);
        assert_eq!(stats.producers, 1);
        assert_eq!(stats.consumers, 2);
        assert_eq!(stats.unresolved, 1);
        assert_eq!(stats.by_protocol, vec![("kafka".to_string(), 3)]);
    }

    #[tokio::test]
    async fn test_missing_table_reads_as_empty() {
        // no-init constructor over a fresh connection: table does not exist.
        let conn = Connection::open_in_memory().unwrap();
        let repo =
            DuckdbChannelEndpointRepository::with_connection_no_init(Arc::new(Mutex::new(conn)));

        assert!(repo.find_by_repository("repo-a").await.unwrap().is_empty());
        assert!(repo.find_all().await.unwrap().is_empty());
        let stats = repo.get_stats("repo-a").await.unwrap();
        assert_eq!(stats.total_endpoints, 0);
    }
}
