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
    ExplainUseCase, ExtractionReport, FileHashRepository, FileRelationshipUseCase,
    GraphExpansionUseCase, ImpactAnalysis, ImpactAnalysisUseCase, ImpactNode, ImportOutcome,
    ImportSessionUseCase, IndexRepositoryUseCase, ListRepositoriesUseCase, MemoryBrowseUseCase,
    MemoryExtractionUseCase, MemoryLevel, MemoryRepository, MemoryRow, MemorySearchUseCase,
    MetadataRepository, ParserService, QueryExpander, RerankingService, ResolveChannelsUseCase,
    ResolvedConfigValue, RowTarget, Scip, SearchCodeUseCase, SnippetLookupUseCase,
    SummarizeMemoryUseCase, SymbolClusterDetectionUseCase, SymbolContext, SymbolContextUseCase,
    VectorRepository, MEMORY_ROOT_URI, RESOURCES_ROOT_URI, SESSIONS_ROOT_URI,
};

pub use application::resource_slug;

pub use application::{aggregate, render, VizFormat, DEFAULT_NODE_LIMIT};

pub use cli::{
    ClustersSubcommand, Commands, EmbeddingTarget, FeaturesSubcommand, LlmTarget, MemorySubcommand,
    OutputFormat, RerankingTarget, SymbolClustersSubcommand, TuiMode,
};

pub use connector::adapter::{
    discover_all_sessions, load_transcript as load_discovered_transcript,
};

pub use connector::adapter::management::{
    routes as management_routes, run_management_server, AppState as ManagementAppState,
};

pub use connector::{
    parse_transcript, parse_transcript_file, AnthropicClient, AnthropicReranking,
    DuckdbAnalysisRepository, DuckdbCallGraphRepository, DuckdbChannelEndpointRepository,
    DuckdbFileHashRepository, DuckdbMemoryRepository, DuckdbMetadataRepository,
    DuckdbVectorRepository, InMemoryVectorRepository, LlmQueryExpander, MockEmbedding,
    MockReranking, NamespaceEmbeddingConfig, NoEmbedding, OpenAiChatClient, OpenAiEmbedding,
    OpenAiReranking, OrtEmbedding, OrtReranking, TreeSitterChannelExtractor, TreeSitterParser,
    DEFAULT_ONNX_EMBEDDING_MODEL, MEMORY_DB_FILE, NO_EMBEDDINGS_MODEL,
};

pub use domain::{
    compute_file_hash, ChannelEdge, ChannelEndpoint, ChannelRole, Cluster, ClusterGraph, CodeChunk,
    DiscoveredSession, DomainError, Embedding, EmbeddingConfig, EndpointSource, ExecutionFeature,
    FeatureNode, FileHash, ImportedSession, IndexingStatus, Language, MemoryItem, MemoryKind,
    MemoryNode, MemoryOperation, NodeKind, NodeType, Protocol, ReferenceKind, Repository,
    SearchQuery, SearchResult, SessionLocator, SessionMessage, SessionSource, SessionTranscript,
    SymbolCommunity, SymbolCommunityGraph, SymbolReference, VectorStore,
};

pub use domain::{CommunityMeta, GraphEdge, GraphLevel, GraphNode, GraphView};

pub use connector::api::{
    namespace_embedding_config, resolve_repo_context, run_import_picker_ui, Container,
    ContainerConfig, MemoryController, ResolvedContext, Router,
};
