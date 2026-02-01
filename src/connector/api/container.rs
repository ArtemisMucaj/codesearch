use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::info;

use crate::application::FileHashRepository;
use crate::{
    CandleEmbedding, CandleReranking, ChromaVectorRepository, DeleteRepositoryUseCase,
    DuckdbFileHashRepository, DuckdbMetadataRepository, DuckdbVectorRepository, EmbeddingService,
    InMemoryVectorRepository, IndexRepositoryUseCase, ListRepositoriesUseCase, MockEmbedding,
    MockReranking, RerankingService, SearchCodeUseCase, TreeSitterParser, VectorRepository,
};

pub struct ContainerConfig {
    pub data_dir: String,
    pub mock_embeddings: bool,
    pub chroma_url: Option<String>,
    pub namespace: String,
    pub memory_storage: bool,
    pub no_rerank: bool,
    /// Force CPU-only inference, disabling automatic GPU detection
    pub cpu_only: bool,
}

pub struct Container {
    parser: Arc<TreeSitterParser>,
    embedding_service: Arc<dyn EmbeddingService>,
    reranking_service: Option<Arc<dyn RerankingService>>,
    vector_repo: Arc<dyn VectorRepository>,
    repo_adapter: Arc<DuckdbMetadataRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    config: ContainerConfig,
}

impl Container {
    pub async fn new(config: ContainerConfig) -> Result<Self> {
        let db_path = PathBuf::from(&config.data_dir).join("codesearch.duckdb");

        // Initialize parser
        let parser = Arc::new(TreeSitterParser::new());

        // Initialize embedding service (Candle with automatic GPU detection)
        let embedding_service: Arc<dyn EmbeddingService> = if config.mock_embeddings {
            info!("Using mock embedding service");
            Arc::new(MockEmbedding::new())
        } else {
            let use_gpu = !config.cpu_only;
            info!(
                "Initializing Candle embedding service (GPU enabled: {})...",
                use_gpu
            );
            Arc::new(CandleEmbedding::new(None, use_gpu)?)
        };

        // Initialize reranking service (Candle with automatic GPU detection)
        let reranking_service: Option<Arc<dyn RerankingService>> = if !config.no_rerank {
            if config.mock_embeddings {
                info!("Using mock reranking service");
                Some(Arc::new(MockReranking::new()))
            } else {
                let use_gpu = !config.cpu_only;
                info!(
                    "Initializing Candle reranking service (GPU enabled: {})...",
                    use_gpu
                );
                match CandleReranking::new(None, use_gpu) {
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

        // Create vector repository, metadata adapter, and file hash repository
        let (vector_repo, repo_adapter, file_hash_repo): (
            Arc<dyn VectorRepository>,
            Arc<DuckdbMetadataRepository>,
            Arc<dyn FileHashRepository>,
        ) = if config.memory_storage {
            info!("Using in-memory vector storage");
            let vector = Arc::new(InMemoryVectorRepository::new());
            let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
            let file_hash_repo = Arc::new(
                DuckdbFileHashRepository::with_connection(repo_adapter.shared_connection()).await?,
            );
            (vector, repo_adapter, file_hash_repo)
        } else if let Some(chroma_url) = config.chroma_url.as_deref() {
            match ChromaVectorRepository::new(chroma_url, &config.namespace).await {
                Ok(chroma) => {
                    info!(
                        "Connected to ChromaDB at {} namespace {}",
                        chroma_url, config.namespace
                    );
                    let vector = Arc::new(chroma);
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    let file_hash_repo = Arc::new(
                        DuckdbFileHashRepository::with_connection(repo_adapter.shared_connection())
                            .await?,
                    );
                    (vector, repo_adapter, file_hash_repo)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to connect to ChromaDB ({}): {}. Falling back to in-memory storage.",
                        chroma_url,
                        e
                    );
                    let vector = Arc::new(InMemoryVectorRepository::new());
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    let file_hash_repo = Arc::new(
                        DuckdbFileHashRepository::with_connection(repo_adapter.shared_connection())
                            .await?,
                    );
                    (vector, repo_adapter, file_hash_repo)
                }
            }
        } else {
            // DuckDB vector storage - share connection with repository adapter
            match DuckdbVectorRepository::new_with_namespace(&db_path, &config.namespace) {
                Ok(duckdb) => {
                    info!(
                        "Using DuckDB vector storage at {:?} namespace {}",
                        db_path, config.namespace
                    );
                    // Share the connection with the repository adapter and file hash repo
                    let shared_conn = duckdb.shared_connection();
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::with_connection(
                        Arc::clone(&shared_conn),
                    )?);
                    let file_hash_repo =
                        Arc::new(DuckdbFileHashRepository::with_connection(shared_conn).await?);
                    (Arc::new(duckdb), repo_adapter, file_hash_repo)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to initialize DuckDB ({}): {}. Falling back to in-memory storage.",
                        db_path.display(),
                        e
                    );
                    let vector = Arc::new(InMemoryVectorRepository::new());
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    let file_hash_repo = Arc::new(
                        DuckdbFileHashRepository::with_connection(repo_adapter.shared_connection())
                            .await?,
                    );
                    (vector, repo_adapter, file_hash_repo)
                }
            }
        };

        Ok(Self {
            parser,
            embedding_service,
            reranking_service,
            vector_repo,
            repo_adapter,
            file_hash_repo,
            config,
        })
    }

    pub fn index_use_case(&self) -> IndexRepositoryUseCase {
        IndexRepositoryUseCase::new(
            self.repo_adapter.clone(),
            self.vector_repo.clone(),
            self.file_hash_repo.clone(),
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
        DeleteRepositoryUseCase::new(self.repo_adapter.clone(), self.vector_repo.clone())
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
