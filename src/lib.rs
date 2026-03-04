pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;

pub use application::{
    CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase, ContextEdge,
    DeleteRepositoryUseCase, EmbeddingService, FileHashRepository, ImpactAnalysis,
    ImpactAnalysisUseCase, ImpactNode, IndexRepositoryUseCase, ListRepositoriesUseCase,
    MetadataRepository, ParserService, QueryExpander, RerankingService, Scip,
    SearchCodeUseCase, SymbolContext, SymbolContextUseCase, VectorRepository,
};

pub use cli::{Commands, EmbeddingTarget, OutputFormat, QueryExpansionTarget, RerankingTarget};

pub use connector::{
    AnthropicClient, AnthropicReranking, ChatClient, DuckdbCallGraphRepository,
    DuckdbFileHashRepository, DuckdbMetadataRepository, DuckdbVectorRepository,
    InMemoryVectorRepository, LlmQueryExpander, MockEmbedding, MockReranking, NamespaceEmbeddingConfig,
    OpenAiChatClient, OpenAiEmbedding, OpenAiReranking, OrtEmbedding, OrtReranking, TreeSitterParser,
};

pub use domain::{
    compute_file_hash, CodeChunk, DomainError, Embedding, EmbeddingConfig, FileHash,
    IndexingStatus, Language, NodeType, ReferenceKind, Repository, SearchQuery, SearchResult,
    SymbolReference, VectorStore,
};

pub use connector::api::{Container, ContainerConfig, Router};
