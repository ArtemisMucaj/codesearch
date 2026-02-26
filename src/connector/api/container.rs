use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::debug;

use crate::application::{
    CallGraphExtractor, CallGraphRepository, CallGraphUseCase, FileHashRepository, ParserService,
    QueryExpander,
};
use crate::connector::adapter::scip::ScipPhaseRunner;
use crate::{
    AnthropicClient, DeleteRepositoryUseCase, DuckdbCallGraphRepository, DuckdbFileHashRepository,
    DuckdbMetadataRepository, DuckdbVectorRepository, EmbeddingService, ImpactAnalysisUseCase,
    InMemoryVectorRepository, IndexRepositoryUseCase, ListRepositoriesUseCase, LlmQueryExpander,
    MockEmbedding, MockReranking, OrtEmbedding, OrtReranking, ParserBasedExtractor,
    RerankingService, ScipPhase, SearchCodeUseCase, SymbolContextUseCase, TreeSitterParser,
    VectorRepository,
};

pub struct ContainerConfig {
    pub data_dir: String,
    pub mock_embeddings: bool,
    pub namespace: String,
    pub memory_storage: bool,
    pub no_rerank: bool,
    /// Open the database in read-only mode.
    ///
    /// When `true`, DuckDB is opened with `AccessMode::ReadOnly`, which does not
    /// acquire the exclusive write lock. This allows multiple `codesearch` processes
    /// (e.g. concurrent search commands) to read the same database file simultaneously.
    ///
    /// Set this to `true` for commands that never write: `search`, `list`, `stats`.
    pub read_only: bool,
    /// Enable query expansion. When `true`, the search query is automatically
    /// expanded into multiple variants before searching so that complementary
    /// results are surfaced and fused via RRF.
    ///
    /// When `true`, uses an LLM-based expander targeting LM Studio on
    /// `http://localhost:1234` by default (no API key required). Override with
    /// `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, and `ANTHROPIC_API_KEY` to point
    /// at any Anthropic-compatible server including the cloud. Falls back to the
    /// original query gracefully when the server is unreachable.
    pub expand_query: bool,
}

pub struct Container {
    parser: Arc<TreeSitterParser>,
    embedding_service: Arc<dyn EmbeddingService>,
    reranking_service: Option<Arc<dyn RerankingService>>,
    query_expander: Option<Arc<dyn QueryExpander>>,
    vector_repo: Arc<dyn VectorRepository>,
    repo_adapter: Arc<DuckdbMetadataRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    call_graph_use_case: Arc<CallGraphUseCase>,
    config: ContainerConfig,
}

/// Initialise the three DuckDB-backed metadata repositories shared across all storage paths.
///
/// When `read_only` is `true`, the metadata database is opened with
/// `AccessMode::ReadOnly` (no exclusive write lock), and the file-hash / call-graph
/// repositories are created without running `CREATE TABLE` DDL (forbidden in
/// read-only mode).  When `false`, the normal writable constructors are used.
async fn init_duckdb_metadata_repos(
    db_path: &std::path::Path,
    read_only: bool,
) -> Result<(
    Arc<DuckdbMetadataRepository>,
    Arc<dyn FileHashRepository>,
    Arc<dyn CallGraphRepository>,
)> {
    let repo_adapter = if read_only {
        Arc::new(DuckdbMetadataRepository::new_read_only(db_path)?)
    } else {
        Arc::new(DuckdbMetadataRepository::new(db_path)?)
    };
    let shared_conn = repo_adapter.shared_connection();
    let file_hash_repo: Arc<dyn FileHashRepository> = if read_only {
        Arc::new(DuckdbFileHashRepository::with_connection_no_init(
            Arc::clone(&shared_conn),
        ))
    } else {
        Arc::new(DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn)).await?)
    };
    let call_graph_repo: Arc<dyn CallGraphRepository> = if read_only {
        Arc::new(DuckdbCallGraphRepository::with_connection_no_init(
            shared_conn,
        ))
    } else {
        Arc::new(DuckdbCallGraphRepository::with_connection(shared_conn).await?)
    };
    Ok((repo_adapter, file_hash_repo, call_graph_repo))
}

impl Container {
    pub async fn new(config: ContainerConfig) -> Result<Self> {
        let db_path = PathBuf::from(&config.data_dir).join("codesearch.duckdb");

        // Initialize parser
        let parser = Arc::new(TreeSitterParser::new());

        // Initialize embedding service
        let embedding_service: Arc<dyn EmbeddingService> = if config.mock_embeddings {
            debug!("Using mock embedding service");
            Arc::new(MockEmbedding::new())
        } else {
            debug!("Initializing ONNX embedding service...");
            Arc::new(OrtEmbedding::new(None)?)
        };

        // Initialize reranking service
        let reranking_service: Option<Arc<dyn RerankingService>> = if !config.no_rerank {
            if config.mock_embeddings {
                debug!("Using mock reranking service");
                Some(Arc::new(MockReranking::new()))
            } else {
                debug!("Initializing ONNX reranking service...");
                match OrtReranking::new(None) {
                    Ok(reranker) => Some(Arc::new(reranker)),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to initialize reranking service: {}. Continuing without reranking.",
                            e
                        );
                        None
                    }
                }
            }
        } else {
            None
        };

        // Create vector repository, metadata adapter, file hash repository, and call graph repository
        let (vector_repo, repo_adapter, file_hash_repo, call_graph_repo): (
            Arc<dyn VectorRepository>,
            Arc<DuckdbMetadataRepository>,
            Arc<dyn FileHashRepository>,
            Arc<dyn CallGraphRepository>,
        ) = if config.memory_storage {
            debug!("Using in-memory vector storage");
            let vector = Arc::new(InMemoryVectorRepository::new());
            let (repo_adapter, file_hash_repo, call_graph_repo) =
                init_duckdb_metadata_repos(&db_path, config.read_only).await?;
            (vector, repo_adapter, file_hash_repo, call_graph_repo)
        } else if config.read_only {
            // Read-only DuckDB path: no exclusive write lock → concurrent searches work
            match DuckdbVectorRepository::new_read_only_with_namespace(&db_path, &config.namespace)
            {
                Ok(duckdb) => {
                    debug!(
                        "Using DuckDB vector storage (read-only) at {:?} namespace {}",
                        db_path, config.namespace
                    );
                    let shared_conn = duckdb.shared_connection();
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::with_connection(
                        Arc::clone(&shared_conn),
                    )?);
                    let file_hash_repo = Arc::new(
                        DuckdbFileHashRepository::with_connection_no_init(Arc::clone(&shared_conn)),
                    );
                    let call_graph_repo = Arc::new(
                        DuckdbCallGraphRepository::with_connection_no_init(shared_conn),
                    );
                    (
                        Arc::new(duckdb),
                        repo_adapter,
                        file_hash_repo,
                        call_graph_repo,
                    )
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to open DuckDB in read-only mode ({}): {}. Falling back to in-memory storage.",
                        db_path.display(),
                        e
                    );
                    // The read-only open already failed (database may not exist yet or is
                    // corrupt). We intentionally open DuckDB metadata in write mode here as
                    // a last-resort degraded fallback. This may fail if another process holds
                    // the write lock, but the vector store is in-memory so no vector writes
                    // will occur.
                    debug!(
                        "Degraded fallback: opening DuckDB metadata in write mode despite \
                        read_only=true (read-only open failed above)"
                    );
                    let vector = Arc::new(InMemoryVectorRepository::new());
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    let shared_conn = repo_adapter.shared_connection();
                    let file_hash_repo = Arc::new(
                        DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn)).await?,
                    );
                    let call_graph_repo =
                        Arc::new(DuckdbCallGraphRepository::with_connection(shared_conn).await?);
                    (vector, repo_adapter, file_hash_repo, call_graph_repo)
                }
            }
        } else {
            // DuckDB vector storage - share connection with repository adapter
            match DuckdbVectorRepository::new_with_namespace(&db_path, &config.namespace) {
                Ok(duckdb) => {
                    debug!(
                        "Using DuckDB vector storage at {:?} namespace {}",
                        db_path, config.namespace
                    );
                    // Share the connection with all adapters
                    let shared_conn = duckdb.shared_connection();
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::with_connection(
                        Arc::clone(&shared_conn),
                    )?);
                    let file_hash_repo = Arc::new(
                        DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn)).await?,
                    );
                    let call_graph_repo =
                        Arc::new(DuckdbCallGraphRepository::with_connection(shared_conn).await?);
                    (
                        Arc::new(duckdb),
                        repo_adapter,
                        file_hash_repo,
                        call_graph_repo,
                    )
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to initialize DuckDB ({}): {}. Falling back to in-memory storage.",
                        db_path.display(),
                        e
                    );
                    let vector = Arc::new(InMemoryVectorRepository::new());
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    let shared_conn = repo_adapter.shared_connection();
                    let file_hash_repo = Arc::new(
                        DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn)).await?,
                    );
                    let call_graph_repo =
                        Arc::new(DuckdbCallGraphRepository::with_connection(shared_conn).await?);
                    (vector, repo_adapter, file_hash_repo, call_graph_repo)
                }
            }
        };

        // Create the call graph use case with the parser-based extractor.
        // ParserBasedExtractor lives in the connector layer (it does file I/O);
        // CallGraphUseCase only knows about the CallGraphExtractor trait.
        let extractor = Arc::new(ParserBasedExtractor::new(
            parser.clone() as Arc<dyn ParserService>
        )) as Arc<dyn CallGraphExtractor>;
        let call_graph_use_case = Arc::new(CallGraphUseCase::new(extractor, call_graph_repo));

        // Initialise the query expander when --expand-query is requested.
        //
        // Local-first: targets LM Studio at http://localhost:1234 by default.
        // Override with ANTHROPIC_BASE_URL / ANTHROPIC_MODEL / ANTHROPIC_API_KEY
        // to point at any other Anthropic-compatible server (including the cloud).
        // If the server is unreachable the expander falls back to the original
        // query gracefully, so search always returns results.
        let query_expander: Option<Arc<dyn QueryExpander>> = if config.expand_query {
            let anthropic = AnthropicClient::from_env();
            debug!(
                "Using LLM-based query expander (url={})",
                anthropic.configured_base_url()
            );
            Some(Arc::new(LlmQueryExpander::new(Arc::new(anthropic))))
        } else {
            None
        };

        Ok(Self {
            parser,
            embedding_service,
            reranking_service,
            query_expander,
            vector_repo,
            repo_adapter,
            file_hash_repo,
            call_graph_use_case,
            config,
        })
    }

    pub fn index_use_case(&self) -> IndexRepositoryUseCase {
        let scip_phase: Arc<dyn ScipPhase> = Arc::new(ScipPhaseRunner);
        IndexRepositoryUseCase::new(
            self.repo_adapter.clone(),
            self.vector_repo.clone(),
            self.file_hash_repo.clone(),
            self.call_graph_use_case.clone(),
            self.parser.clone(),
            self.embedding_service.clone(),
        )
        .with_scip_phase(scip_phase)
    }

    pub fn search_use_case(&self) -> SearchCodeUseCase {
        let mut use_case =
            SearchCodeUseCase::new(self.vector_repo.clone(), self.embedding_service.clone());

        if let Some(reranker) = self.reranking_service.clone() {
            use_case = use_case.with_reranking(reranker);
        }

        if let Some(expander) = self.query_expander.clone() {
            use_case = use_case.with_query_expansion(expander);
        }

        use_case
    }

    pub fn list_use_case(&self) -> ListRepositoriesUseCase {
        ListRepositoriesUseCase::new(self.repo_adapter.clone())
    }

    pub fn delete_use_case(&self) -> DeleteRepositoryUseCase {
        DeleteRepositoryUseCase::new(
            self.repo_adapter.clone(),
            self.vector_repo.clone(),
            self.file_hash_repo.clone(),
            self.call_graph_use_case.clone(),
        )
    }

    /// Get the call graph use case for direct access to call graph functionality.
    pub fn call_graph_use_case(&self) -> Arc<CallGraphUseCase> {
        self.call_graph_use_case.clone()
    }

    pub fn impact_use_case(&self) -> ImpactAnalysisUseCase {
        ImpactAnalysisUseCase::new(self.call_graph_use_case.clone())
    }

    pub fn context_use_case(&self) -> SymbolContextUseCase {
        SymbolContextUseCase::new(self.call_graph_use_case.clone())
    }

    pub fn data_dir(&self) -> &str {
        &self.config.data_dir
    }

    pub fn namespace(&self) -> &str {
        &self.config.namespace
    }

    pub fn memory_storage(&self) -> bool {
        self.config.memory_storage
    }
}
