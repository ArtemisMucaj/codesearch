use std::sync::Arc;

use async_trait::async_trait;
use duckdb::{params, Connection};
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::{CallGraphQuery, CallGraphRepository, CallGraphStats};
use crate::domain::{DomainError, Language, ReferenceKind, SymbolReference};

pub struct DuckdbCallGraphRepository {
    conn: Arc<Mutex<Connection>>,
}

impl DuckdbCallGraphRepository {
    /// Create a new adapter using an existing shared connection.
    pub async fn with_connection(conn: Arc<Mutex<Connection>>) -> Result<Self, DomainError> {
        let conn_guard = conn.lock().await;
        Self::initialize_schema(&conn_guard)?;
        drop(conn_guard);

        Ok(Self { conn })
    }

    /// Create a new adapter from a shared connection without running schema initialization.
    ///
    /// Use this when the connection is read-only (DDL is forbidden) or when the
    /// schema is guaranteed to already exist. The symbol_references table is never
    /// queried during read-only operations (search / list / stats), so this is safe.
    pub fn with_connection_no_init(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    fn initialize_schema(conn: &Connection) -> Result<(), DomainError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS symbol_references (
                id TEXT PRIMARY KEY,
                caller_symbol TEXT,
                callee_symbol TEXT NOT NULL,
                caller_file_path TEXT NOT NULL,
                reference_file_path TEXT NOT NULL,
                reference_line INTEGER NOT NULL,
                reference_column INTEGER NOT NULL,
                reference_kind TEXT NOT NULL,
                language TEXT NOT NULL,
                repository_id TEXT NOT NULL,
                caller_node_type TEXT,
                enclosing_scope TEXT,
                import_alias TEXT
            );

            -- Migrate existing databases: add import_alias column if absent.
            ALTER TABLE symbol_references ADD COLUMN IF NOT EXISTS import_alias TEXT;

            -- Index for finding callers of a symbol
            CREATE INDEX IF NOT EXISTS idx_symbol_refs_callee
            ON symbol_references(callee_symbol, repository_id);

            -- Index for finding what a symbol calls
            CREATE INDEX IF NOT EXISTS idx_symbol_refs_caller
            ON symbol_references(caller_symbol, repository_id);

            -- Index for finding references by file
            CREATE INDEX IF NOT EXISTS idx_symbol_refs_file
            ON symbol_references(reference_file_path, repository_id);

            -- Index for repository-wide operations
            CREATE INDEX IF NOT EXISTS idx_symbol_refs_repo
            ON symbol_references(repository_id);

            -- Index for cross-repository lookups
            CREATE INDEX IF NOT EXISTS idx_symbol_refs_callee_all
            ON symbol_references(callee_symbol);

            -- Index for language filtering
            CREATE INDEX IF NOT EXISTS idx_symbol_refs_language
            ON symbol_references(language, repository_id);
            "#,
        )
        .map_err(|e| {
            DomainError::storage(format!("Failed to initialize symbol_references schema: {}", e))
        })?;

        debug!("DuckDB symbol_references table initialized");
        Ok(())
    }

    fn row_to_symbol_reference(row: &duckdb::Row<'_>) -> duckdb::Result<SymbolReference> {
        Ok(SymbolReference::reconstitute(
            row.get::<_, String>(0)?,                                  // id
            row.get::<_, Option<String>>(1)?,                          // caller_symbol
            row.get::<_, String>(2)?,                                  // callee_symbol
            row.get::<_, String>(3)?,                                  // caller_file_path
            row.get::<_, String>(4)?,                                  // reference_file_path
            row.get::<_, i32>(5)? as u32,                              // reference_line
            row.get::<_, i32>(6)? as u32,                              // reference_column
            ReferenceKind::parse(&row.get::<_, String>(7)?),          // reference_kind
            Language::parse(&row.get::<_, String>(8)?),               // language
            row.get::<_, String>(9)?,                                  // repository_id
            row.get::<_, Option<String>>(10)?,                         // caller_node_type
            row.get::<_, Option<String>>(11)?,                         // enclosing_scope
            row.get::<_, Option<String>>(12)?,                         // import_alias
        ))
    }

    fn build_where_clause(query: &CallGraphQuery, base_conditions: &str) -> String {
        let mut conditions = vec![base_conditions.to_string()];

        if query.repository_id.is_some() {
            conditions.push("repository_id = ?".to_string());
        }
        if query.language.is_some() {
            conditions.push("language = ?".to_string());
        }
        if query.reference_kind.is_some() {
            conditions.push("reference_kind = ?".to_string());
        }

        conditions.join(" AND ")
    }
}

#[async_trait]
impl CallGraphRepository for DuckdbCallGraphRepository {
    async fn save_batch(&self, references: &[SymbolReference]) -> Result<(), DomainError> {
        if references.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .map_err(|e| DomainError::storage(format!("Failed to begin transaction: {}", e)))?;

        {
            let mut stmt = tx
                .prepare(
                    r#"INSERT INTO symbol_references (
                        id, caller_symbol, callee_symbol, caller_file_path,
                        reference_file_path, reference_line, reference_column,
                        reference_kind, language, repository_id,
                        caller_node_type, enclosing_scope, import_alias
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT (id) DO UPDATE SET
                        caller_symbol = excluded.caller_symbol,
                        callee_symbol = excluded.callee_symbol,
                        caller_file_path = excluded.caller_file_path,
                        reference_file_path = excluded.reference_file_path,
                        reference_line = excluded.reference_line,
                        reference_column = excluded.reference_column,
                        reference_kind = excluded.reference_kind,
                        language = excluded.language,
                        repository_id = excluded.repository_id,
                        caller_node_type = excluded.caller_node_type,
                        enclosing_scope = excluded.enclosing_scope,
                        import_alias = excluded.import_alias
                    "#,
                )
                .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

            for reference in references {
                stmt.execute(params![
                    reference.id(),
                    reference.caller_symbol(),
                    reference.callee_symbol(),
                    reference.caller_file_path(),
                    reference.reference_file_path(),
                    reference.reference_line() as i32,
                    reference.reference_column() as i32,
                    reference.reference_kind().as_str(),
                    reference.language().as_str(),
                    reference.repository_id(),
                    reference.caller_node_type(),
                    reference.enclosing_scope(),
                    reference.import_alias(),
                ])
                .map_err(|e| {
                    DomainError::storage(format!("Failed to save symbol reference: {}", e))
                })?;
            }
        }

        tx.commit()
            .map_err(|e| DomainError::storage(format!("Failed to commit: {}", e)))?;

        debug!("Saved {} symbol references to DuckDB", references.len());
        Ok(())
    }

    async fn find_callers(
        &self,
        callee_symbol: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        let conn = self.conn.lock().await;

        let where_clause = Self::build_where_clause(query, "callee_symbol = ?");
        let limit_clause = query.limit.map_or(String::new(), |l| format!(" LIMIT {}", l));

        let sql = format!(
            r#"SELECT id, caller_symbol, callee_symbol, caller_file_path,
                      reference_file_path, reference_line, reference_column,
                      reference_kind, language, repository_id,
                      caller_node_type, enclosing_scope, import_alias
               FROM symbol_references
               WHERE {}
               ORDER BY reference_file_path, reference_line{}"#,
            where_clause, limit_clause
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        // Build parameter list dynamically
        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        params_vec.push(Box::new(callee_symbol.to_string()));

        if let Some(ref repo_id) = query.repository_id {
            params_vec.push(Box::new(repo_id.clone()));
        }
        if let Some(ref lang) = query.language {
            params_vec.push(Box::new(lang.clone()));
        }
        if let Some(ref kind) = query.reference_kind {
            params_vec.push(Box::new(kind.clone()));
        }

        let params_refs: Vec<&dyn duckdb::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), Self::row_to_symbol_reference)
            .map_err(|e| DomainError::storage(format!("Failed to query callers: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            results
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }

        Ok(results)
    }

    async fn find_callees(
        &self,
        caller_symbol: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        let conn = self.conn.lock().await;

        let where_clause = Self::build_where_clause(query, "caller_symbol = ?");
        let limit_clause = query.limit.map_or(String::new(), |l| format!(" LIMIT {}", l));

        let sql = format!(
            r#"SELECT id, caller_symbol, callee_symbol, caller_file_path,
                      reference_file_path, reference_line, reference_column,
                      reference_kind, language, repository_id,
                      caller_node_type, enclosing_scope, import_alias
               FROM symbol_references
               WHERE {}
               ORDER BY reference_file_path, reference_line{}"#,
            where_clause, limit_clause
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        params_vec.push(Box::new(caller_symbol.to_string()));

        if let Some(ref repo_id) = query.repository_id {
            params_vec.push(Box::new(repo_id.clone()));
        }
        if let Some(ref lang) = query.language {
            params_vec.push(Box::new(lang.clone()));
        }
        if let Some(ref kind) = query.reference_kind {
            params_vec.push(Box::new(kind.clone()));
        }

        let params_refs: Vec<&dyn duckdb::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), Self::row_to_symbol_reference)
            .map_err(|e| DomainError::storage(format!("Failed to query callees: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            results
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }

        Ok(results)
    }

    async fn find_by_file(
        &self,
        file_path: &str,
        query: &CallGraphQuery,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        let conn = self.conn.lock().await;

        let where_clause = Self::build_where_clause(query, "reference_file_path = ?");
        let limit_clause = query.limit.map_or(String::new(), |l| format!(" LIMIT {}", l));

        let sql = format!(
            r#"SELECT id, caller_symbol, callee_symbol, caller_file_path,
                      reference_file_path, reference_line, reference_column,
                      reference_kind, language, repository_id,
                      caller_node_type, enclosing_scope, import_alias
               FROM symbol_references
               WHERE {}
               ORDER BY reference_line{}"#,
            where_clause, limit_clause
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        params_vec.push(Box::new(file_path.to_string()));

        if let Some(ref repo_id) = query.repository_id {
            params_vec.push(Box::new(repo_id.clone()));
        }
        if let Some(ref lang) = query.language {
            params_vec.push(Box::new(lang.clone()));
        }
        if let Some(ref kind) = query.reference_kind {
            params_vec.push(Box::new(kind.clone()));
        }

        let params_refs: Vec<&dyn duckdb::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), Self::row_to_symbol_reference)
            .map_err(|e| DomainError::storage(format!("Failed to query by file: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            results
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }

        Ok(results)
    }

    async fn find_by_repository(
        &self,
        repository_id: &str,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                r#"SELECT id, caller_symbol, callee_symbol, caller_file_path,
                          reference_file_path, reference_line, reference_column,
                          reference_kind, language, repository_id,
                          caller_node_type, enclosing_scope, import_alias
                   FROM symbol_references
                   WHERE repository_id = ?
                   ORDER BY reference_file_path, reference_line"#,
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let rows = stmt
            .query_map(params![repository_id], Self::row_to_symbol_reference)
            .map_err(|e| DomainError::storage(format!("Failed to query by repository: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            results
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }

        Ok(results)
    }

    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError> {
        let conn = self.conn.lock().await;

        // First count how many we're deleting
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbol_references WHERE repository_id = ? AND reference_file_path = ?",
                params![repository_id, file_path],
                |row| row.get(0),
            )
            .map_err(|e| DomainError::storage(format!("Failed to count references: {}", e)))?;

        conn.execute(
            "DELETE FROM symbol_references WHERE repository_id = ? AND reference_file_path = ?",
            params![repository_id, file_path],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete references: {}", e)))?;

        debug!(
            "Deleted {} symbol references for file {} in repository {}",
            count, file_path, repository_id
        );
        Ok(count as u64)
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM symbol_references WHERE repository_id = ?",
            params![repository_id],
        )
        .map_err(|e| DomainError::storage(format!("Failed to delete references: {}", e)))?;

        debug!(
            "Deleted all symbol references for repository {}",
            repository_id
        );
        Ok(())
    }

    async fn get_stats(&self, repository_id: &str) -> Result<CallGraphStats, DomainError> {
        let conn = self.conn.lock().await;

        // Total references
        let total_references: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbol_references WHERE repository_id = ?",
                params![repository_id],
                |row| row.get(0),
            )
            .map_err(|e| DomainError::storage(format!("Failed to count references: {}", e)))?;

        // Unique callers
        let unique_callers: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT caller_symbol) FROM symbol_references WHERE repository_id = ? AND caller_symbol IS NOT NULL",
                params![repository_id],
                |row| row.get(0),
            )
            .map_err(|e| DomainError::storage(format!("Failed to count unique callers: {}", e)))?;

        // Unique callees
        let unique_callees: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT callee_symbol) FROM symbol_references WHERE repository_id = ?",
                params![repository_id],
                |row| row.get(0),
            )
            .map_err(|e| DomainError::storage(format!("Failed to count unique callees: {}", e)))?;

        // By reference kind
        let mut stmt = conn
            .prepare(
                "SELECT reference_kind, COUNT(*) FROM symbol_references WHERE repository_id = ? GROUP BY reference_kind ORDER BY COUNT(*) DESC",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let by_kind_rows = stmt
            .query_map(params![repository_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query by kind: {}", e)))?;

        let mut by_reference_kind = Vec::new();
        for row in by_kind_rows {
            let (kind, count) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            by_reference_kind.push((kind, count as u64));
        }

        // By language
        let mut stmt = conn
            .prepare(
                "SELECT language, COUNT(*) FROM symbol_references WHERE repository_id = ? GROUP BY language ORDER BY COUNT(*) DESC",
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let by_lang_rows = stmt
            .query_map(params![repository_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| DomainError::storage(format!("Failed to query by language: {}", e)))?;

        let mut by_language = Vec::new();
        for row in by_lang_rows {
            let (lang, count) =
                row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?;
            by_language.push((lang, count as u64));
        }

        Ok(CallGraphStats {
            total_references: total_references as u64,
            unique_callers: unique_callers as u64,
            unique_callees: unique_callees as u64,
            by_reference_kind,
            by_language,
        })
    }

    async fn find_cross_repo_references(
        &self,
        symbol_name: &str,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                r#"SELECT id, caller_symbol, callee_symbol, caller_file_path,
                          reference_file_path, reference_line, reference_column,
                          reference_kind, language, repository_id,
                          caller_node_type, enclosing_scope, import_alias
                   FROM symbol_references
                   WHERE callee_symbol = ?
                   ORDER BY repository_id, reference_file_path, reference_line"#,
            )
            .map_err(|e| DomainError::storage(format!("Failed to prepare statement: {}", e)))?;

        let rows = stmt
            .query_map(params![symbol_name], Self::row_to_symbol_reference)
            .map_err(|e| {
                DomainError::storage(format!("Failed to query cross-repo references: {}", e))
            })?;

        let mut results = Vec::new();
        for row in rows {
            results
                .push(row.map_err(|e| DomainError::storage(format!("Failed to read row: {}", e)))?);
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_repo() -> DuckdbCallGraphRepository {
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        DuckdbCallGraphRepository::with_connection(conn)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_save_and_find_callers() {
        let repo = create_test_repo().await;

        let reference = SymbolReference::new(
            Some("my_function".to_string()),
            "other_function".to_string(),
            "src/lib.rs".to_string(),
            "src/lib.rs".to_string(),
            42,
            10,
            ReferenceKind::Call,
            Language::Rust,
            "repo-123".to_string(),
        );

        repo.save_batch(&[reference]).await.unwrap();

        let query = CallGraphQuery::new().with_repository("repo-123");
        let callers = repo.find_callers("other_function", &query).await.unwrap();

        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].caller_symbol(), Some("my_function"));
        assert_eq!(callers[0].callee_symbol(), "other_function");
    }

    #[tokio::test]
    async fn test_find_callees() {
        let repo = create_test_repo().await;

        let references = vec![
            SymbolReference::new(
                Some("my_function".to_string()),
                "helper1".to_string(),
                "src/lib.rs".to_string(),
                "src/lib.rs".to_string(),
                10,
                5,
                ReferenceKind::Call,
                Language::Rust,
                "repo-123".to_string(),
            ),
            SymbolReference::new(
                Some("my_function".to_string()),
                "helper2".to_string(),
                "src/lib.rs".to_string(),
                "src/lib.rs".to_string(),
                15,
                5,
                ReferenceKind::Call,
                Language::Rust,
                "repo-123".to_string(),
            ),
        ];

        repo.save_batch(&references).await.unwrap();

        let query = CallGraphQuery::new().with_repository("repo-123");
        let callees = repo.find_callees("my_function", &query).await.unwrap();

        assert_eq!(callees.len(), 2);
    }

    #[tokio::test]
    async fn test_get_stats() {
        let repo = create_test_repo().await;

        let references = vec![
            SymbolReference::new(
                Some("func1".to_string()),
                "helper".to_string(),
                "src/a.rs".to_string(),
                "src/a.rs".to_string(),
                10,
                5,
                ReferenceKind::Call,
                Language::Rust,
                "repo-123".to_string(),
            ),
            SymbolReference::new(
                Some("func2".to_string()),
                "helper".to_string(),
                "src/b.rs".to_string(),
                "src/b.rs".to_string(),
                20,
                5,
                ReferenceKind::Call,
                Language::Rust,
                "repo-123".to_string(),
            ),
            SymbolReference::new(
                Some("func1".to_string()),
                "MyType".to_string(),
                "src/a.rs".to_string(),
                "src/a.rs".to_string(),
                5,
                10,
                ReferenceKind::TypeReference,
                Language::Rust,
                "repo-123".to_string(),
            ),
        ];

        repo.save_batch(&references).await.unwrap();

        let stats = repo.get_stats("repo-123").await.unwrap();

        assert_eq!(stats.total_references, 3);
        assert_eq!(stats.unique_callers, 2); // func1, func2
        assert_eq!(stats.unique_callees, 2); // helper, MyType
    }

    #[tokio::test]
    async fn test_delete_by_file_path() {
        let repo = create_test_repo().await;

        let references = vec![
            SymbolReference::new(
                Some("func1".to_string()),
                "helper".to_string(),
                "src/a.rs".to_string(),
                "src/a.rs".to_string(),
                10,
                5,
                ReferenceKind::Call,
                Language::Rust,
                "repo-123".to_string(),
            ),
            SymbolReference::new(
                Some("func2".to_string()),
                "helper".to_string(),
                "src/b.rs".to_string(),
                "src/b.rs".to_string(),
                20,
                5,
                ReferenceKind::Call,
                Language::Rust,
                "repo-123".to_string(),
            ),
        ];

        repo.save_batch(&references).await.unwrap();

        let deleted = repo
            .delete_by_file_path("repo-123", "src/a.rs")
            .await
            .unwrap();
        assert_eq!(deleted, 1);

        let remaining = repo.find_by_repository("repo-123").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].reference_file_path(), "src/b.rs");
    }
}
