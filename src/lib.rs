pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;
pub mod tui;

pub use application::{
    AnalysisRepository, CallGraphNode, CallGraphQuery, CallGraphRepository, CallGraphStats,
    CallGraphUseCase, ChannelEndpointRepository, ChannelExtractor, ChannelLinkOptions,
    ChannelLinkReport, ChannelLinkUseCase, ChannelOverview, ChannelResolver, ChatClient,
    ClusterDetectionUseCase, CommunityNamingUseCase, CouplingDetectionUseCase,
    DeleteRepositoryUseCase, EmbeddingService, ExecutionFeaturesUseCase, ExplainResult,
    ExplainUseCase, FileHashRepository, FileRelationshipUseCase, GraphExpansionUseCase,
    ImpactAnalysis, ImpactAnalysisUseCase, IndexRepositoryUseCase, LanguageShare,
    ListRepositoriesUseCase, MetadataRepository, ModuleDependency, ModuleOverview, NamingRegistry,
    OverviewOptions, OverviewReport, OverviewStats, ParserService, QueryExpander,
    RepositoryOverviewUseCase, RerankingService, ResolveChannelsUseCase, ResolvedConfigValue, Scip,
    SearchCodeUseCase, SkippedSection, SnippetLookupUseCase, SymbolClusterDetectionUseCase,
    SymbolContext, SymbolContextUseCase, VectorRepository,
};

pub use application::{aggregate, render, VizFormat, DEFAULT_NODE_LIMIT};

pub use cli::{
    ClustersSubcommand, Commands, CopilotSubcommand, EmbeddingTarget, FeaturesSubcommand,
    LlmTarget, OpenaiSubcommand, OutputFormat, RerankingTarget, SymbolClustersSubcommand, TuiMode,
};

pub use connector::adapter::management::{
    read_runinfo, remove_runinfo, routes as management_routes, run_management_server,
    write_runinfo, AppState as ManagementAppState, ServeRunInfo,
};

pub use connector::{
    AnthropicClient, AnthropicReranking, CodesearchConfig, CopilotChatClient,
    DuckdbAnalysisRepository, DuckdbCallGraphRepository, DuckdbChannelEndpointRepository,
    DuckdbFileHashRepository, DuckdbMetadataRepository, DuckdbVectorRepository,
    InMemoryVectorRepository, LlmQueryExpander, MockEmbedding, MockReranking,
    NamespaceEmbeddingConfig, NamespaceInfo, NoEmbedding, OpenAiChatClient, OpenAiEmbedding,
    OpenAiReranking, OrtEmbedding, OrtReranking, TreeSitterChannelExtractor, TreeSitterParser,
    DEFAULT_ONNX_EMBEDDING_MODEL, NO_EMBEDDINGS_MODEL,
};

pub use domain::{
    compute_file_hash, namespace_scope_id, stable_community_id, ChannelEdge, ChannelEndpoint,
    ChannelRole, Cluster, ClusterGraph, CodeChunk, CommunityCoupling, CouplingElement,
    CouplingElementKind, CouplingReport, DomainError, Embedding, EmbeddingConfig, EndpointSource,
    ExecutionFeature, FeatureNode, FileHash, IndexingStatus, Language, NodeType, Protocol,
    ReferenceKind, Repository, SearchQuery, SearchResult, SymbolCommunity, SymbolCommunityGraph,
    SymbolReference, VectorStore, NAMESPACE_SCOPE_ID,
};

pub use domain::{CommunityMeta, GraphEdge, GraphLevel, GraphNode, GraphView};

pub use connector::api::{
    namespace_embedding_config, resolve_repo_context, run_copilot_command, run_openai_command,
    Container, ContainerConfig, ResolvedContext, Router,
};
