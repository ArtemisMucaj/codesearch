//! SQLite-based storage for code chunks and repository metadata.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::domain::{
    ChunkRepository, CodeChunk, DomainError, Language, NodeType, Repository, RepositoryRepository,
};

/// SQLite-based storage.
pub struct SqliteStorage {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStorage {
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

            CREATE TABLE IF NOT EXISTS code_chunks (
                id TEXT PRIMARY KEY,
                file_path TEXT NOT NULL,
                content TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                language TEXT NOT NULL,
                node_type TEXT NOT NULL,
                symbol_name TEXT,
                parent_symbol TEXT,
                repository_id TEXT NOT NULL,
                FOREIGN KEY (repository_id) REFERENCES repositories(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_repository ON code_chunks(repository_id);
            CREATE INDEX IF NOT EXISTS idx_chunks_file ON code_chunks(file_path);
            CREATE INDEX IF NOT EXISTS idx_chunks_language ON code_chunks(language);
            "#,
        )
        .map_err(|e| DomainError::storage(format!("Failed to initialize schema: {}", e)))?;

        debug!("Database schema initialized");
        Ok(())
    }
}

#[async_trait]
impl RepositoryRepository for SqliteStorage {
    async fn save(&self, repository: &Repository) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;

        conn.execute(
            r#"INSERT OR REPLACE INTO repositories (id, name, path, created_at, updated_at, chunk_count, file_count)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                repository.id, repository.name, repository.path,
                repository.created_at, repository.updated_at,
                repository.chunk_count, repository.file_count,
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

        let result = stmt
            .query_row(params![id], |row| {
                Ok(Repository {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    chunk_count: row.get(5)?,
                    file_count: row.get(6)?,
                })
            })
            .ok();

        Ok(result)
    }

    async fn find_by_path(&self, path: &str) -> Result<Option<Repository>, DomainError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare("SELECT id, name, path, created_at, updated_at, chunk_count, file_count FROM repositories WHERE path = ?1")
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let result = stmt.query_row(params![path], |row| {
            Ok(Repository {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                chunk_count: row.get(5)?,
                file_count: row.get(6)?,
            })
        }).ok();

        Ok(result)
    }

    async fn list(&self) -> Result<Vec<Repository>, DomainError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare("SELECT id, name, path, created_at, updated_at, chunk_count, file_count FROM repositories ORDER BY name")
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(Repository {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    chunk_count: row.get(5)?,
                    file_count: row.get(6)?,
                })
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

#[async_trait]
impl ChunkRepository for SqliteStorage {
    async fn save(&self, chunk: &CodeChunk) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;

        conn.execute(
            r#"INSERT OR REPLACE INTO code_chunks
               (id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
            params![
                chunk.id, chunk.file_path, chunk.content,
                chunk.start_line, chunk.end_line,
                chunk.language.as_str(), chunk.node_type.as_str(),
                chunk.symbol_name, chunk.parent_symbol, chunk.repository_id,
            ],
        )
        .map_err(|e| DomainError::storage(format!("Failed to save chunk: {}", e)))?;

        Ok(())
    }

    async fn save_batch(&self, chunks: &[CodeChunk]) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;

        let tx = conn.unchecked_transaction()
            .map_err(|e| DomainError::storage(format!("Failed to start transaction: {}", e)))?;

        {
            let mut stmt = tx.prepare(
                r#"INSERT OR REPLACE INTO code_chunks
                   (id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
            ).map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

            for chunk in chunks {
                stmt.execute(params![
                    chunk.id, chunk.file_path, chunk.content,
                    chunk.start_line, chunk.end_line,
                    chunk.language.as_str(), chunk.node_type.as_str(),
                    chunk.symbol_name, chunk.parent_symbol, chunk.repository_id,
                ]).map_err(|e| DomainError::storage(format!("Failed to insert chunk: {}", e)))?;
            }
        }

        tx.commit().map_err(|e| DomainError::storage(format!("Failed to commit transaction: {}", e)))?;

        Ok(())
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn.prepare(
            r#"SELECT id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id
               FROM code_chunks WHERE id = ?1"#,
        ).map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let result = stmt.query_row(params![id], |row| {
            Ok(CodeChunk {
                id: row.get(0)?,
                file_path: row.get(1)?,
                content: row.get(2)?,
                start_line: row.get(3)?,
                end_line: row.get(4)?,
                language: parse_language(row.get::<_, String>(5)?.as_str()),
                node_type: parse_node_type(row.get::<_, String>(6)?.as_str()),
                symbol_name: row.get(7)?,
                parent_symbol: row.get(8)?,
                repository_id: row.get(9)?,
            })
        }).ok();

        Ok(result)
    }

    async fn find_by_file(&self, file_path: &str) -> Result<Vec<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;
        query_chunks(&conn, "SELECT id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id FROM code_chunks WHERE file_path = ?1", params![file_path])
    }

    async fn find_by_repository(&self, repository_id: &str) -> Result<Vec<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;
        query_chunks(&conn, "SELECT id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id FROM code_chunks WHERE repository_id = ?1", params![repository_id])
    }

    async fn find_by_language(&self, language: Language) -> Result<Vec<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;
        query_chunks(&conn, "SELECT id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id FROM code_chunks WHERE language = ?1", params![language.as_str()])
    }

    async fn find_by_node_type(&self, node_type: NodeType) -> Result<Vec<CodeChunk>, DomainError> {
        let conn = self.conn.lock().await;
        query_chunks(&conn, "SELECT id, file_path, content, start_line, end_line, language, node_type, symbol_name, parent_symbol, repository_id FROM code_chunks WHERE node_type = ?1", params![node_type.as_str()])
    }

    async fn delete(&self, id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM code_chunks WHERE id = ?1", params![id])
            .map_err(|e| DomainError::storage(format!("Failed to delete chunk: {}", e)))?;
        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM code_chunks WHERE repository_id = ?1", params![repository_id])
            .map_err(|e| DomainError::storage(format!("Failed to delete chunks: {}", e)))?;
        Ok(())
    }

    async fn delete_by_file(&self, file_path: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM code_chunks WHERE file_path = ?1", params![file_path])
            .map_err(|e| DomainError::storage(format!("Failed to delete chunks: {}", e)))?;
        Ok(())
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_chunks", [], |row| row.get(0))
            .map_err(|e| DomainError::storage(format!("Failed to count chunks: {}", e)))?;
        Ok(count as u64)
    }

    async fn count_by_repository(&self, repository_id: &str) -> Result<u64, DomainError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_chunks WHERE repository_id = ?1", params![repository_id], |row| row.get(0))
            .map_err(|e| DomainError::storage(format!("Failed to count chunks: {}", e)))?;
        Ok(count as u64)
    }
}

fn query_chunks(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<CodeChunk>, DomainError> {
    let mut stmt = conn.prepare(sql)
        .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

    let rows = stmt.query_map(params, |row| {
        Ok(CodeChunk {
            id: row.get(0)?,
            file_path: row.get(1)?,
            content: row.get(2)?,
            start_line: row.get(3)?,
            end_line: row.get(4)?,
            language: parse_language(row.get::<_, String>(5)?.as_str()),
            node_type: parse_node_type(row.get::<_, String>(6)?.as_str()),
            symbol_name: row.get(7)?,
            parent_symbol: row.get(8)?,
            repository_id: row.get(9)?,
        })
    }).map_err(|e| DomainError::storage(format!("Failed to query chunks: {}", e)))?;

    let mut chunks = Vec::new();
    for row in rows {
        chunks.push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
    }

    Ok(chunks)
}

fn parse_language(s: &str) -> Language {
    match s {
        "rust" => Language::Rust,
        "python" => Language::Python,
        "javascript" => Language::JavaScript,
        "typescript" => Language::TypeScript,
        "go" => Language::Go,
        _ => Language::Unknown,
    }
}

fn parse_node_type(s: &str) -> NodeType {
    match s {
        "function" => NodeType::Function,
        "class" => NodeType::Class,
        "struct" => NodeType::Struct,
        "enum" => NodeType::Enum,
        "trait" => NodeType::Trait,
        "impl" => NodeType::Impl,
        "module" => NodeType::Module,
        "constant" => NodeType::Constant,
        "typedef" => NodeType::TypeDef,
        _ => NodeType::Block,
    }
}
