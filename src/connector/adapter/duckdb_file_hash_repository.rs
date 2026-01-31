use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::FileHashRepository;
use crate::domain::{DomainError, FileHash};

pub struct DuckdbFileHashRepository {
    conn: Arc<Mutex<Connection>>,
}

impl DuckdbFileHashRepository {
    /// Create a new adapter using an existing shared connection.
    pub async fn with_connection(conn: Arc<Mutex<Connection>>) -> Result<Self, DomainError> {
        // Initialize the file_hashes table
        let conn_guard = conn.lock().await;
        Self::initialize_schema(&conn_guard)?;
        drop(conn_guard);

        Ok(Self { conn })
    }

    fn initialize_schema(conn: &Connection) -> Result<(), DomainError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS file_hashes (
                file_path TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                repository_id TEXT NOT NULL,
                PRIMARY KEY (repository_id, file_path)
            );

            CREATE INDEX IF NOT EXISTS idx_file_hashes_repo
            ON file_hashes(repository_id);
            "#,
        )
        .map_err(|e| {
            DomainError::storage(format!("Failed to initialize file_hashes schema: {}", e))
        })?;

        debug!("DuckDB file_hashes table initialized");
        Ok(())
    }
}

#[async_trait]
impl FileHashRepository for DuckdbFileHashRepository {
    async fn save_batch(&self, hashes: &[FileHash]) -> Result<(), DomainError> {
        if hashes.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO file_hashes (file_path, content_hash, repository_id) \
                     VALUES (?, ?, ?)",
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

            for hash in hashes {
                stmt.execute(params![
                    hash.file_path(),
                    hash.content_hash(),
                    hash.repository_id(),
                ])
                .map_err(|e| DomainError::storage(format!("Failed to save file hash: {}", e)))?;
            }
        }

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!("Saved {} file hashes to DuckDB", hashes.len());
        Ok(())
    }

    async fn find_by_repository(&self, repository_id: &str) -> Result<Vec<FileHash>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT file_path, content_hash, repository_id FROM file_hashes WHERE repository_id = ?",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let rows = stmt
            .query_map(params![repository_id], |row| {
                Ok(FileHash::new(
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query file hashes: {}", e)))?;

        let mut hashes = Vec::new();
        for row in rows {
            hashes
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }

        Ok(hashes)
    }

    async fn delete_by_paths(
        &self,
        repository_id: &str,
        paths: &[String],
    ) -> Result<(), DomainError> {
        if paths.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        {
            let mut stmt = tx
                .prepare("DELETE FROM file_hashes WHERE repository_id = ? AND file_path = ?")
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

            for path in paths {
                stmt.execute(params![repository_id, path]).map_err(|e| {
                    DomainError::storage(format!("Failed to delete file hash: {}", e))
                })?;
            }
        }

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!("Deleted {} file hashes from DuckDB", paths.len());
        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM file_hashes WHERE repository_id = ?",
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete file hashes: {}", e)))?;

        debug!("Deleted all file hashes for repository {}", repository_id);
        Ok(())
    }
}
