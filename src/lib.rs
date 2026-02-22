pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;

pub use application::{
    CallGraphExtractor, CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase,
    ContextEdge, DeleteRepositoryUseCase, EmbeddingService, FileHashRepository,
    ImpactAnalysis, ImpactAnalysisUseCase, ImpactNode, IndexRepositoryUseCase,
    ListRepositoriesUseCase, MetadataRepository, ParserBasedExtractor, ParserService,
    QueryExpander, RerankingService, SearchCodeUseCase, SymbolContext, SymbolContextUseCase,
    VectorRepository,
};

pub use cli::{Commands, OutputFormat};

pub use connector::{
    DuckdbCallGraphRepository, DuckdbFileHashRepository,
    DuckdbMetadataRepository, DuckdbVectorRepository, InMemoryVectorRepository, LlmQueryExpander,
    MockEmbedding, MockReranking, OrtEmbedding, OrtReranking, TreeSitterParser,
};

pub use domain::{
    compute_file_hash, CodeChunk, DomainError, Embedding, EmbeddingConfig, FileHash,
    IndexingStatus, Language, NodeType, ReferenceKind, Repository, SearchQuery, SearchResult,
    SymbolReference, VectorStore,
};

pub use connector::api::{Container, ContainerConfig, Router};
