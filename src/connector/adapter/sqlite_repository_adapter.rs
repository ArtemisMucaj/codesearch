use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::RepositoryRepository;
use crate::domain::{DomainError, Repository};

pub struct SqliteRepositoryAdapter {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteRepositoryAdapter {
    pub fn new(db_path: &Path) -> Result<Self, DomainError> {
        let conn = Connection::open(db_path)
            .map_err(|e| DomainError::storage(format!("Failed to open database: {}", e)))?;

        let storage = Self {
            conn: Arc::new(Mutex::new(conn)),
        };

        tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(storage.initialize_schema())
        })?;

        Ok(storage)
    }

    pub fn in_memory() -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| DomainError::storage(format!("Failed to create in-memory database: {}", e)))?;

        let storage = Self {
            conn: Arc::new(Mutex::new(conn)),
        };

        tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(storage.initialize_schema())
        })?;

        Ok(storage)
    }

    async fn initialize_schema(&self) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS repositories (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                chunk_count INTEGER DEFAULT 0,
                file_count INTEGER DEFAULT 0
            );

            "#,
        )
        .map_err(|e| DomainError::storage(format!("Failed to initialize schema: {}", e)))?;

        debug!("Database schema initialized");
        Ok(())
    }
}

#[async_trait]
impl RepositoryRepository for SqliteRepositoryAdapter {
    async fn save(&self, repository: &Repository) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;

        conn.execute(
            r#"INSERT OR REPLACE INTO repositories (id, name, path, created_at, updated_at, chunk_count, file_count)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                repository.id(),
                repository.name(),
                repository.path(),
                repository.created_at(),
                repository.updated_at(),
                repository.chunk_count(),
                repository.file_count(),
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to save repository: {}", e)))?;

        Ok(())
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<Repository>, DomainError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare("SELECT id, name, path, created_at, updated_at, chunk_count, file_count FROM repositories WHERE id = ?1")
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        match stmt.query_row(params![id], |row| {
            Ok(Repository::reconstitute(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        }) {
            Ok(repo) => Ok(Some(repo)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!("Failed to query repository: {}", e))),
        }
    }

    async fn find_by_path(&self, path: &str) -> Result<Option<Repository>, DomainError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare("SELECT id, name, path, created_at, updated_at, chunk_count, file_count FROM repositories WHERE path = ?1")
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        match stmt.query_row(params![path], |row| {
            Ok(Repository::reconstitute(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        }) {
            Ok(repo) => Ok(Some(repo)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!("Failed to query repository by path: {}", e))),
        }
    }

    async fn list(&self) -> Result<Vec<Repository>, DomainError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare("SELECT id, name, path, created_at, updated_at, chunk_count, file_count FROM repositories ORDER BY name")
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(Repository::reconstitute(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query repositories: {}", e)))?;

        let mut repos = Vec::new();
        for row in rows {
            repos.push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }

        Ok(repos)
    }

    async fn delete(&self, id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM repositories WHERE id = ?1", params![id])
            .map_err(|e| DomainError::storage(format!("Failed to delete repository: {}", e)))?;
        Ok(())
    }

    async fn update_stats(&self, id: &str, chunk_count: u64, file_count: u64) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        conn.execute(
            "UPDATE repositories SET chunk_count = ?1, file_count = ?2, updated_at = ?3 WHERE id = ?4",
            params![chunk_count, file_count, now, id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to update repository stats: {}", e)))?;

        Ok(())
    }
}
