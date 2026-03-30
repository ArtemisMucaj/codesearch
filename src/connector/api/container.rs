use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, warn};

use crate::application::{
    CallGraphRepository, CallGraphUseCase, FileHashRepository, QueryExpander,
};
use crate::cli::{EmbeddingTarget, LlmTarget, RerankingTarget};
use crate::connector::adapter::scip::ScipRunner;
use crate::connector::adapter::NamespaceEmbeddingConfig;
use crate::{
    AnthropicClient, AnthropicReranking, DeleteRepositoryUseCase, DuckdbCallGraphRepository,
    DuckdbFileHashRepository, DuckdbMetadataRepository, DuckdbVectorRepository, EmbeddingService,
    ExplainUseCase, FileRelationshipUseCase, ImpactAnalysisUseCase, InMemoryVectorRepository,
    IndexRepositoryUseCase, ListRepositoriesUseCase, LlmQueryExpander, MockEmbedding,
    MockReranking, OpenAiChatClient, OpenAiEmbedding, OpenAiReranking, OrtEmbedding, OrtReranking,
    RerankingService, Scip, SearchCodeUseCase, SnippetLookupUseCase, SymbolContextUseCase,
    TreeSitterParser, VectorRepository,
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
    /// Which embedding backend to use.
    ///
    /// `Onnx` (default): bundled ONNX models downloaded from HuggingFace.
    /// `Api`: OpenAI-compatible `/v1/embeddings` endpoint (e.g. LM Studio).
    /// Set `OPENAI_BASE_URL` to override the default `http://localhost:1234`.
    ///
    /// The chosen target and model are stored in `namespace_config` on first use
    /// and validated on every subsequent open — mismatches are hard errors.
    pub embedding_target: EmbeddingTarget,
    /// Which reranking backend to use (when reranking is enabled).
    ///
    /// `Onnx` (default): bundled ONNX cross-encoder model.
    /// `ApiAnthropic`: LLM via Anthropic-compatible `/v1/messages` — uses
    ///   `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, `ANTHROPIC_API_KEY`.
    /// `ApiOpenAi`: LLM via OpenAI-compatible `/v1/chat/completions` — uses
    ///   `OPENAI_BASE_URL`, `OPENAI_MODEL`, `OPENAI_API_KEY`.
    pub reranking_target: RerankingTarget,
    /// Which provider to use for LLM-based query expansion (when enabled).
    ///
    /// `Anthropic` (default): Anthropic-compatible `/v1/messages` — uses
    ///   `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, `ANTHROPIC_API_KEY`.
    /// `OpenAi`: OpenAI-compatible `/v1/chat/completions` — uses
    ///   `OPENAI_BASE_URL`, `OPENAI_MODEL`, `OPENAI_API_KEY`.
    pub llm_target: LlmTarget,
    /// Embedding model identifier.
    ///
    /// For `Onnx`: HuggingFace model ID (default: `sentence-transformers/all-MiniLM-L6-v2`).
    /// For `Api`: model name sent in the `/v1/embeddings` request body (must match
    /// the model currently loaded in LM Studio or the target server).
    ///
    /// `None` means "use the default for the selected target".
    pub embedding_model: Option<String>,
    /// Number of dimensions produced by the embedding model.
    ///
    /// Defaults to 384 (the dimension of `all-MiniLM-L6-v2`).  Override with
    /// `--embedding-dimensions` when using a model with a different output size.
    /// The value is persisted in `namespace_config` and cannot change after the
    /// namespace has been indexed — use a different namespace or re-index with
    /// `--force` to change it.
    pub embedding_dimensions: usize,
    /// Maximum number of `embed_chunks` calls issued concurrently during indexing.
    ///
    /// For the API embedding target each call is a network round-trip, so higher
    /// values (4–8) dramatically reduce wall-clock indexing time at the cost of
    /// more simultaneous HTTP connections to the embedding server.
    ///
    /// For the ONNX target each call becomes a `spawn_blocking` task.  The
    /// effective throughput gain is bounded by the number of physical CPU cores;
    /// setting this above `num_cpus` just adds scheduling overhead without
    /// improving speed.
    ///
    /// Default: 4.
    pub parse_concurrency: usize,
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

/// Maximum number of retry attempts when a read-only DuckDB open fails due to a
/// lock held by a concurrent writer (e.g. an ongoing `codesearch index` run).
const READ_ONLY_LOCK_RETRIES: u32 = 5;

/// Initial backoff delay for lock-conflict retries.  Doubles on each attempt:
/// 500 ms → 1 s → 2 s → 4 s → 8 s  (≈ 15.5 s total wait before giving up).
const READ_ONLY_LOCK_RETRY_INITIAL_MS: u64 = 500;

/// Returns `true` when the error string looks like a DuckDB file-lock conflict
/// produced by a concurrent writer process.
fn is_lock_conflict(err: &str) -> bool {
    err.contains("Could not set lock on file") || err.contains("Conflicting lock is held")
}

/// Attempt to open the DuckDB vector repository in read-only mode, retrying
/// with exponential backoff when the failure is a cross-process lock conflict.
///
/// A single warning is emitted on the first lock conflict so the user knows the
/// tool is waiting.  After [`READ_ONLY_LOCK_RETRIES`] failed attempts the last
/// error is returned as-is so the caller can surface a clear message.
async fn open_read_only_with_retry(
    db_path: &std::path::Path,
    namespace: &str,
    ns_cfg: &NamespaceEmbeddingConfig,
) -> Result<DuckdbVectorRepository, crate::domain::DomainError> {
    let mut delay_ms = READ_ONLY_LOCK_RETRY_INITIAL_MS;
    for attempt in 0..=READ_ONLY_LOCK_RETRIES {
        match DuckdbVectorRepository::new_read_only_with_namespace(db_path, namespace, ns_cfg) {
            Ok(repo) => return Ok(repo),
            Err(e) if attempt < READ_ONLY_LOCK_RETRIES && is_lock_conflict(&e.to_string()) => {
                if attempt == 0 {
                    warn!("DuckDB is locked by another process; waiting for it to finish…",);
                }
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

impl Container {
    pub async fn new(config: ContainerConfig) -> Result<Self> {
        let db_path = PathBuf::from(&config.data_dir).join("codesearch.duckdb");

        // Initialize parser
        let parser = Arc::new(TreeSitterParser::new());

        // Resolve the effective model name for the selected embedding target.
        const ONNX_DEFAULT_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";
        let effective_model = match config.embedding_model.clone() {
            Some(m) => m,
            None => match config.embedding_target {
                EmbeddingTarget::Onnx => ONNX_DEFAULT_MODEL.to_string(),
                EmbeddingTarget::Api => {
                    return Err(anyhow::anyhow!(
                        "--embedding-model is required when using --embedding-target=api"
                    ));
                }
            },
        };

        // Initialize embedding service
        let embedding_service: Arc<dyn EmbeddingService> = if config.mock_embeddings {
            debug!("Using mock embedding service");
            Arc::new(MockEmbedding::new())
        } else {
            match config.embedding_target {
                EmbeddingTarget::Onnx => {
                    debug!(
                        "Initializing ONNX embedding service (model='{}')...",
                        effective_model
                    );
                    let model_arg = if effective_model == ONNX_DEFAULT_MODEL {
                        None
                    } else {
                        Some(effective_model.as_str())
                    };
                    Arc::new(OrtEmbedding::new(model_arg)?)
                }
                EmbeddingTarget::Api => {
                    debug!(
                        "Using OpenAI embedding service (model='{}', dims={})",
                        effective_model, config.embedding_dimensions
                    );
                    Arc::new(OpenAiEmbedding::new(
                        effective_model.clone(),
                        config.embedding_dimensions,
                    ))
                }
            }
        };

        // Initialize reranking service
        let reranking_service: Option<Arc<dyn RerankingService>> = if !config.no_rerank {
            if config.mock_embeddings {
                debug!("Using mock reranking service");
                Some(Arc::new(MockReranking::new()))
            } else {
                match config.reranking_target {
                    RerankingTarget::Onnx => {
                        debug!("Initializing ONNX reranking service...");
                        match OrtReranking::new(None) {
                            Ok(reranker) => Some(Arc::new(reranker)),
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to initialize ONNX reranking service: {}. \
                                     Continuing without reranking.",
                                    e
                                );
                                None
                            }
                        }
                    }
                    RerankingTarget::ApiAnthropic => {
                        debug!("Using Anthropic reranking service (/v1/messages)");
                        let client = Arc::new(AnthropicClient::from_env());
                        Some(Arc::new(AnthropicReranking::new(client)))
                    }
                    RerankingTarget::ApiOpenAi => {
                        debug!("Using OpenAI reranking service (/v1/chat/completions)");
                        let client = Arc::new(OpenAiChatClient::from_env()?);
                        Some(Arc::new(OpenAiReranking::new(client)))
                    }
                }
            }
        } else {
            None
        };

        // Build the NamespaceEmbeddingConfig that is stored/validated per namespace.
        let ns_cfg = NamespaceEmbeddingConfig {
            embedding_target: match config.embedding_target {
                EmbeddingTarget::Onnx => "onnx".to_string(),
                EmbeddingTarget::Api => "api".to_string(),
            },
            embedding_model: effective_model,
            dimensions: config.embedding_dimensions,
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
            // Read-only DuckDB path: no exclusive write lock → concurrent searches work.
            // Retry with exponential backoff when the database is temporarily locked by a
            // concurrent indexing process so the user doesn't silently get empty results.
            match open_read_only_with_retry(&db_path, &config.namespace, &ns_cfg).await {
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
                    let msg = e.to_string();
                    if is_lock_conflict(&msg) {
                        // A concurrent indexing process is still holding the write lock after
                        // all retries.  Return a clear error rather than silently serving
                        // empty in-memory results.
                        return Err(anyhow::anyhow!(
                            "Cannot open the database for searching: another process is currently \
                             indexing ({db}). Please wait for indexing to finish and try again.\n\
                             Details: {msg}",
                            db = db_path.display(),
                            msg = msg,
                        ));
                    }
                    // Any other failure (e.g. database does not exist yet, corrupt file).
                    // Degrade to in-memory storage so `codesearch search` doesn't hard-crash
                    // on a fresh install.
                    warn!(
                        "Failed to open DuckDB in read-only mode ({}): {}. \
                         Falling back to in-memory storage.",
                        db_path.display(),
                        msg
                    );
                    let vector = Arc::new(InMemoryVectorRepository::new());
                    let (repo_adapter, file_hash_repo, call_graph_repo) =
                        init_duckdb_metadata_repos(&db_path, false).await?;
                    (vector, repo_adapter, file_hash_repo, call_graph_repo)
                }
            }
        } else {
            // DuckDB vector storage - share connection with repository adapter
            match DuckdbVectorRepository::new_with_namespace(&db_path, &config.namespace, &ns_cfg) {
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

        let call_graph_use_case = Arc::new(CallGraphUseCase::new(call_graph_repo));

        // Initialise the query expander when --expand-query is requested.
        // Falls back gracefully to the original query when the server is unreachable.
        let query_expander: Option<Arc<dyn QueryExpander>> = if config.expand_query {
            let client: Arc<dyn crate::connector::adapter::ChatClient> = match config.llm_target {
                LlmTarget::Anthropic => {
                    let c = AnthropicClient::from_env();
                    debug!(
                        "Using Anthropic query expander (url={})",
                        c.configured_base_url()
                    );
                    Arc::new(c)
                }
                LlmTarget::OpenAi => {
                    let c = OpenAiChatClient::from_env()?;
                    debug!(
                        "Using OpenAI query expander (url={})",
                        c.configured_base_url()
                    );
                    Arc::new(c)
                }
            };
            Some(Arc::new(LlmQueryExpander::new(client)))
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
        let scip: Arc<dyn Scip> = Arc::new(ScipRunner);
        IndexRepositoryUseCase::new(
            self.repo_adapter.clone(),
            self.vector_repo.clone(),
            self.file_hash_repo.clone(),
            self.call_graph_use_case.clone(),
            self.parser.clone(),
            self.embedding_service.clone(),
        )
        .with_scip(scip)
        .with_parse_concurrency(self.config.parse_concurrency)
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

    pub fn metadata_repository(&self) -> Arc<dyn crate::application::MetadataRepository> {
        self.repo_adapter.clone()
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

    pub fn snippet_lookup_use_case(&self) -> SnippetLookupUseCase {
        SnippetLookupUseCase::new(self.vector_repo.clone())
    }

    pub fn explain_use_case(&self) -> ExplainUseCase {
        ExplainUseCase::new(
            Arc::new(self.context_use_case()),
            self.snippet_lookup_use_case(),
        )
    }

    pub fn file_graph_use_case(&self) -> FileRelationshipUseCase {
        FileRelationshipUseCase::new(
            self.call_graph_use_case.clone(),
            self.vector_repo.clone(),
            self.metadata_repository(),
        )
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
