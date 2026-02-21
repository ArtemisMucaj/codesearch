use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::debug;

use crate::application::{CallGraphRepository, CallGraphUseCase, FileHashRepository, ParserService};
use crate::{
    ChromaVectorRepository, DeleteRepositoryUseCase, DuckdbCallGraphRepository,
    DuckdbFileHashRepository, DuckdbMetadataRepository, DuckdbVectorRepository, EmbeddingService,
    InMemoryVectorRepository, IndexRepositoryUseCase, ListRepositoriesUseCase, MockEmbedding,
    MockReranking, OrtEmbedding, OrtReranking, RerankingService, SearchCodeUseCase,
    TreeSitterParser, VectorRepository,
};

pub struct ContainerConfig {
    pub data_dir: String,
    pub mock_embeddings: bool,
    pub chroma_url: Option<String>,
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
}

pub struct Container {
    parser: Arc<TreeSitterParser>,
    embedding_service: Arc<dyn EmbeddingService>,
    reranking_service: Option<Arc<dyn RerankingService>>,
    vector_repo: Arc<dyn VectorRepository>,
    repo_adapter: Arc<DuckdbMetadataRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    call_graph_use_case: Arc<CallGraphUseCase>,
    config: ContainerConfig,
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
            let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
            let shared_conn = repo_adapter.shared_connection();
            let file_hash_repo = Arc::new(
                DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn)).await?,
            );
            let call_graph_repo =
                Arc::new(DuckdbCallGraphRepository::with_connection(shared_conn).await?);
            (vector, repo_adapter, file_hash_repo, call_graph_repo)
        } else if let Some(chroma_url) = config.chroma_url.as_deref() {
            match ChromaVectorRepository::new(chroma_url, &config.namespace).await {
                Ok(chroma) => {
                    debug!(
                        "Connected to ChromaDB at {} namespace {}",
                        chroma_url, config.namespace
                    );
                    let vector = Arc::new(chroma);
                    let repo_adapter = if config.read_only {
                        Arc::new(DuckdbMetadataRepository::new_read_only(&db_path)?)
                    } else {
                        Arc::new(DuckdbMetadataRepository::new(&db_path)?)
                    };
                    let shared_conn = repo_adapter.shared_connection();
                    let file_hash_repo = if config.read_only {
                        Arc::new(DuckdbFileHashRepository::with_connection_no_init(Arc::clone(&shared_conn)))
                            as Arc<dyn crate::application::FileHashRepository>
                    } else {
                        Arc::new(DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn)).await?)
                            as Arc<dyn crate::application::FileHashRepository>
                    };
                    let call_graph_repo = if config.read_only {
                        Arc::new(DuckdbCallGraphRepository::with_connection_no_init(shared_conn))
                            as Arc<dyn CallGraphRepository>
                    } else {
                        Arc::new(DuckdbCallGraphRepository::with_connection(shared_conn).await?)
                            as Arc<dyn CallGraphRepository>
                    };
                    (vector, repo_adapter, file_hash_repo, call_graph_repo)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to connect to ChromaDB ({}): {}. Falling back to in-memory storage.",
                        chroma_url,
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
        } else if config.read_only {
            // Read-only DuckDB path: no exclusive write lock → concurrent searches work
            match DuckdbVectorRepository::new_read_only_with_namespace(&db_path, &config.namespace) {
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
                    (Arc::new(duckdb), repo_adapter, file_hash_repo, call_graph_repo)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to open DuckDB in read-only mode ({}): {}. Falling back to in-memory storage.",
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
                    (Arc::new(duckdb), repo_adapter, file_hash_repo, call_graph_repo)
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

        // Create the call graph use case with parser-based extractor
        let call_graph_use_case = Arc::new(CallGraphUseCase::with_parser(
            parser.clone() as Arc<dyn ParserService>,
            call_graph_repo,
        ));

        Ok(Self {
            parser,
            embedding_service,
            reranking_service,
            vector_repo,
            repo_adapter,
            file_hash_repo,
            call_graph_use_case,
            config,
        })
    }

    pub fn index_use_case(&self) -> IndexRepositoryUseCase {
        IndexRepositoryUseCase::new(
            self.repo_adapter.clone(),
            self.vector_repo.clone(),
            self.file_hash_repo.clone(),
            self.call_graph_use_case.clone(),
            self.parser.clone(),
            self.embedding_service.clone(),
        )
    }

    pub fn search_use_case(&self) -> SearchCodeUseCase {
        let mut use_case =
            SearchCodeUseCase::new(self.vector_repo.clone(), self.embedding_service.clone());

        if let Some(reranker) = self.reranking_service.clone() {
            use_case = use_case.with_reranking(reranker);
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

    pub fn data_dir(&self) -> &str {
        &self.config.data_dir
    }

    pub fn namespace(&self) -> &str {
        &self.config.namespace
    }

    pub fn memory_storage(&self) -> bool {
        self.config.memory_storage
    }

    pub fn chroma_url(&self) -> Option<&str> {
        self.config.chroma_url.as_deref()
    }
}
