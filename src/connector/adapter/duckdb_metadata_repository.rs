use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::MetadataRepository;
use crate::domain::{DomainError, LanguageStats, Repository, VectorStore};

pub struct DuckdbMetadataRepository {
    conn: Arc<Mutex<Connection>>,
}

impl DuckdbMetadataRepository {
    pub fn new(db_path: &Path) -> Result<Self, DomainError> {
        let conn = Connection::open(db_path)
            .map_err(|e| DomainError::storage(format!("Failed to open DuckDB database: {}", e)))?;
        Self::initialize_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create a new adapter using an existing shared connection.
    /// This is useful when multiple adapters need to share the same DuckDB file
    /// (DuckDB only allows one write connection per file).
    ///
    /// The connection must have been initialized with the repositories table already created
    /// (typically by DuckdbVectorRepository::initialize).
    pub fn with_connection(conn: Arc<Mutex<Connection>>) -> Result<Self, DomainError> {
        Ok(Self { conn })
    }

    /// Returns a clone of the shared connection Arc.
    /// This allows other adapters to share the same DuckDB connection.
    pub fn shared_connection(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    #[allow(dead_code)]
    pub fn in_memory() -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            DomainError::storage(format!("Failed to open DuckDB in-memory DB: {}", e))
        })?;
        Self::initialize_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn initialize_schema(conn: &Connection) -> Result<(), DomainError> {
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
            "#,
        )
        .map_err(|e| DomainError::storage(format!("Failed to initialize schema: {}", e)))?;

        debug!("DuckDB repository schema initialized");
        Ok(())
    }

    fn serialize_languages(languages: &HashMap<String, LanguageStats>) -> Option<String> {
        if languages.is_empty() {
            None
        } else {
            serde_json::to_string(languages).ok()
        }
    }

    fn deserialize_languages(json: Option<String>) -> HashMap<String, LanguageStats> {
        json.and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

#[async_trait]
impl MetadataRepository for DuckdbMetadataRepository {
    async fn save(&self, repository: &Repository) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        let languages_json = Self::serialize_languages(repository.languages());

        conn.execute(
            r#"
            INSERT INTO repositories (id, name, path, created_at, updated_at, chunk_count, file_count, store, namespace, languages)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT (id) DO UPDATE SET
                name = excluded.name,
                path = excluded.path,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                chunk_count = excluded.chunk_count,
                file_count = excluded.file_count,
                store = excluded.store,
                namespace = excluded.namespace,
                languages = excluded.languages
            "#,
            params![
                repository.id(),
                repository.name(),
                repository.path(),
                repository.created_at(),
                repository.updated_at(),
                repository.chunk_count() as i64,
                repository.file_count() as i64,
                repository.store().as_str(),
                repository.namespace(),
                languages_json,
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to save repository: {}", e)))?;

        Ok(())
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<Repository>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, path, created_at, updated_at, chunk_count, file_count, store, namespace, languages FROM repositories WHERE id = ?1",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        match stmt.query_row(params![id], |row| {
            let store_str: String = row
                .get::<_, Option<String>>(7)?
                .unwrap_or_else(|| "duckdb".to_string());
            let namespace: Option<String> = row.get(8)?;
            let languages_json: Option<String> = row.get(9)?;
            Ok(Repository::reconstitute(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get::<_, i64>(5)? as u64,
                row.get::<_, i64>(6)? as u64,
                VectorStore::from_str(&store_str),
                namespace,
                Self::deserialize_languages(languages_json),
            ))
        }) {
            Ok(repo) => Ok(Some(repo)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!(
                "Failed to query repository: {}",
                e
            ))),
        }
    }

    async fn find_by_path(&self, path: &str) -> Result<Option<Repository>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, path, created_at, updated_at, chunk_count, file_count, store, namespace, languages FROM repositories WHERE path = ?1",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        match stmt.query_row(params![path], |row| {
            let store_str: String = row
                .get::<_, Option<String>>(7)?
                .unwrap_or_else(|| "duckdb".to_string());
            let namespace: Option<String> = row.get(8)?;
            let languages_json: Option<String> = row.get(9)?;
            Ok(Repository::reconstitute(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get::<_, i64>(5)? as u64,
                row.get::<_, i64>(6)? as u64,
                VectorStore::from_str(&store_str),
                namespace,
                Self::deserialize_languages(languages_json),
            ))
        }) {
            Ok(repo) => Ok(Some(repo)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DomainError::storage(format!(
                "Failed to query repository by path: {}",
                e
            ))),
        }
    }

    async fn list(&self) -> Result<Vec<Repository>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, path, created_at, updated_at, chunk_count, file_count, store, namespace, languages FROM repositories ORDER BY name",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                let store_str: String = row
                    .get::<_, Option<String>>(7)?
                    .unwrap_or_else(|| "duckdb".to_string());
                let namespace: Option<String> = row.get(8)?;
                let languages_json: Option<String> = row.get(9)?;
                Ok(Repository::reconstitute(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get::<_, i64>(5)? as u64,
                    row.get::<_, i64>(6)? as u64,
                    VectorStore::from_str(&store_str),
                    namespace,
                    Self::deserialize_languages(languages_json),
                ))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query repositories: {}", e)))?;

        let mut repos = Vec::new();
        for row in rows {
            repos
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }
        Ok(repos)
    }

    async fn delete(&self, id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM repositories WHERE id = ?1", params![id])
            .map_err(|e| DomainError::storage(format!("Failed to delete repository: {}", e)))?;
        Ok(())
    }

    async fn update_stats(
        &self,
        id: &str,
        chunk_count: u64,
        file_count: u64,
    ) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        conn.execute(
            "UPDATE repositories SET chunk_count = ?1, file_count = ?2, updated_at = ?3 WHERE id = ?4",
            params![chunk_count as i64, file_count as i64, now, id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to update repository stats: {}", e)))?;

        Ok(())
    }

    async fn update_languages(
        &self,
        id: &str,
        languages: HashMap<String, LanguageStats>,
    ) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let languages_json = Self::serialize_languages(&languages);

        conn.execute(
            "UPDATE repositories SET languages = ?1, updated_at = ?2 WHERE id = ?3",
            params![languages_json, now, id],
        )
        .map_err(|e| {
            DomainError::storage(format!("Failed to update repository languages: {}", e))
        })?;

        Ok(())
    }
}
