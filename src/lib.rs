pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;
pub mod tui;

pub use application::{
    CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase, ChatClient, ContextNode,
    DeleteRepositoryUseCase, EmbeddingService, ExplainResult, ExplainUseCase, FileHashRepository,
    FileRelationshipUseCase, ImpactAnalysis, ImpactAnalysisUseCase, ImpactNode,
    IndexRepositoryUseCase, ListRepositoriesUseCase, MetadataRepository, ParserService,
    QueryExpander, RerankingService, Scip, SearchCodeUseCase, SnippetLookupUseCase, SymbolContext,
    SymbolContextUseCase, VectorRepository,
};

pub use cli::{
    ClusterMode, Commands, EmbeddingTarget, GraphFormat, LlmTarget, NodeGranularity, OutputFormat,
    RerankingTarget, TuiMode,
};

pub use connector::{
    AnthropicClient, AnthropicReranking, DuckdbCallGraphRepository, DuckdbFileHashRepository,
    DuckdbMetadataRepository, DuckdbVectorRepository, InMemoryVectorRepository, LlmQueryExpander,
    MockEmbedding, MockReranking, NamespaceEmbeddingConfig, OpenAiChatClient, OpenAiEmbedding,
    OpenAiReranking, OrtEmbedding, OrtReranking, TreeSitterParser,
};

pub use domain::{
    compute_file_hash, CodeChunk, DomainError, Embedding, EmbeddingConfig, FileEdge, FileGraph,
    FileGraphRepo, FileHash, IndexingStatus, Language, NodeType, ReferenceKind, Repository,
    SearchQuery, SearchResult, SymbolReference, VectorStore,
};

pub use connector::api::{Container, ContainerConfig, Router};
