pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;
pub mod tui;

pub use application::{
    CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase, ChatClient,
    ClusterDetectionUseCase, ContextNode, DeleteRepositoryUseCase, EmbeddingService,
    ExecutionFeaturesUseCase, ExplainResult, ExplainUseCase, FileHashRepository,
    FileRelationshipUseCase, ImpactAnalysis, ImpactAnalysisUseCase, ImpactNode,
    IndexRepositoryUseCase, ListRepositoriesUseCase, MetadataRepository, ParserService,
    QueryExpander, RerankingService, Scip, SearchCodeUseCase, SnippetLookupUseCase,
    SymbolContext, SymbolContextUseCase, VectorRepository,
};

pub use cli::{
    ClusterOutputFormat, Commands, ClustersSubcommand, EmbeddingTarget, FeaturesSubcommand,
    LlmTarget, OutputFormat, RerankingTarget, TuiMode,
};

pub use connector::{
    AnthropicClient, AnthropicReranking, DuckdbCallGraphRepository, DuckdbFileHashRepository,
    DuckdbMetadataRepository, DuckdbVectorRepository, InMemoryVectorRepository, LlmQueryExpander,
    MockEmbedding, MockReranking, NamespaceEmbeddingConfig, OpenAiChatClient, OpenAiEmbedding,
    OpenAiReranking, OrtEmbedding, OrtReranking, TreeSitterParser,
};

pub use domain::{
    compute_file_hash, Cluster, ClusterGraph, CodeChunk, DomainError, Embedding, EmbeddingConfig,
    ExecutionFeature, FeatureNode, FileHash, IndexingStatus, Language, NodeType, ReferenceKind,
    Repository, SearchQuery, SearchResult, SymbolReference, VectorStore,
};

pub use connector::api::{Container, ContainerConfig, Router};
