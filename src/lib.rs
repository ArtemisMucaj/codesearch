pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;
pub mod tui;

pub use application::{
    CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase, ChatClient,
    ClusterDetectionUseCase, ContextNode, DeleteRepositoryUseCase, EmbeddingService,
    ExecutionFeaturesUseCase, ExplainResult, ExplainUseCase, FileHashRepository,
    FileRelationshipUseCase, GraphExpansionUseCase, ImpactAnalysis, ImpactAnalysisUseCase,
    ImpactNode, IndexRepositoryUseCase, ListRepositoriesUseCase, MetadataRepository, ParserService,
    QueryExpander, RerankingService, Scip, SearchCodeUseCase, SnippetLookupUseCase,
    SymbolClusterDetectionUseCase, SymbolContext, SymbolContextUseCase, VectorRepository,
};

pub use application::{aggregate, render, VizFormat, DEFAULT_NODE_LIMIT};

pub use cli::{
    ClustersSubcommand, Commands, EmbeddingTarget, FeaturesSubcommand, LlmTarget, OutputFormat,
    RerankingTarget, SymbolClustersSubcommand, TuiMode,
};

pub use connector::{
    AnthropicClient, AnthropicReranking, DuckdbCallGraphRepository, DuckdbFileHashRepository,
    DuckdbMetadataRepository, DuckdbVectorRepository, InMemoryVectorRepository, LlmQueryExpander,
    MockEmbedding, MockReranking, NamespaceEmbeddingConfig, NoEmbedding, OpenAiChatClient,
    OpenAiEmbedding, OpenAiReranking, OrtEmbedding, OrtReranking, TreeSitterParser,
    DEFAULT_ONNX_EMBEDDING_MODEL, NO_EMBEDDINGS_MODEL,
};

pub use domain::{
    compute_file_hash, Cluster, ClusterGraph, CodeChunk, DomainError, Embedding, EmbeddingConfig,
    ExecutionFeature, FeatureNode, FileHash, IndexingStatus, Language, NodeType, ReferenceKind,
    Repository, SearchQuery, SearchResult, SymbolCommunity, SymbolCommunityGraph, SymbolReference,
    VectorStore,
};

pub use domain::{CommunityMeta, GraphEdge, GraphLevel, GraphNode, GraphView};

pub use connector::api::{
    namespace_embedding_config, resolve_repo_context, Container, ContainerConfig, ResolvedContext,
    Router,
};
