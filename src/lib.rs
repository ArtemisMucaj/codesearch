pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;
pub mod tui;

pub use application::{
    AnalysisRepository, CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase,
    ChannelEndpointRepository, ChannelExtractor, ChannelLinkOptions, ChannelLinkReport,
    ChannelLinkUseCase, ChannelResolver, ChatClient, ClusterDetectionUseCase, ContextNode,
    DeleteRepositoryUseCase, EmbeddingService, ExecutionFeaturesUseCase, ExplainResult,
    ExplainUseCase, FileHashRepository, FileRelationshipUseCase, GraphExpansionUseCase,
    ImpactAnalysis, ImpactAnalysisUseCase, ImpactNode, IndexRepositoryUseCase,
    ListRepositoriesUseCase, MetadataRepository, ParserService, QueryExpander, RerankingService,
    ResolveChannelsUseCase, ResolvedConfigValue, Scip, SearchCodeUseCase, SnippetLookupUseCase,
    SymbolClusterDetectionUseCase, SymbolContext, SymbolContextUseCase, VectorRepository,
};

pub use application::{aggregate, render, VizFormat, DEFAULT_NODE_LIMIT};

pub use cli::{
    ClustersSubcommand, Commands, EmbeddingTarget, FeaturesSubcommand, LlmTarget, OutputFormat,
    RerankingTarget, SymbolClustersSubcommand, TuiMode,
};

pub use connector::{
    AnthropicClient, AnthropicReranking, DuckdbAnalysisRepository, DuckdbCallGraphRepository,
    DuckdbChannelEndpointRepository, DuckdbFileHashRepository, DuckdbMetadataRepository,
    DuckdbVectorRepository, InMemoryVectorRepository, LlmQueryExpander, MockEmbedding,
    MockReranking, NamespaceEmbeddingConfig, NoEmbedding, OpenAiChatClient, OpenAiEmbedding,
    OpenAiReranking, OrtEmbedding, OrtReranking, TreeSitterChannelExtractor, TreeSitterParser,
    DEFAULT_ONNX_EMBEDDING_MODEL, NO_EMBEDDINGS_MODEL,
};

pub use domain::{
    compute_file_hash, ChannelEdge, ChannelEndpoint, ChannelRole, Cluster, ClusterGraph, CodeChunk,
    DomainError, Embedding, EmbeddingConfig, EndpointSource, ExecutionFeature, FeatureNode,
    FileHash, IndexingStatus, Language, NodeType, Protocol, ReferenceKind, Repository, SearchQuery,
    SearchResult, SymbolCommunity, SymbolCommunityGraph, SymbolReference, VectorStore,
};

pub use domain::{CommunityMeta, GraphEdge, GraphLevel, GraphNode, GraphView};

pub use connector::api::{
    namespace_embedding_config, resolve_repo_context, Container, ContainerConfig, ResolvedContext,
    Router,
};
