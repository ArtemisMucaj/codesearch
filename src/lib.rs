pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;

pub use application::{
    CallGraphExtractor, CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase,
    DeleteRepositoryUseCase, EmbeddingService, FileHashRepository, IndexRepositoryUseCase,
    ListRepositoriesUseCase, MetadataRepository, ParserBasedExtractor, ParserService,
    RerankingService, SearchCodeUseCase, VectorRepository,
};

pub use cli::Commands;

pub use connector::{
    ChromaVectorRepository, DuckdbCallGraphRepository, DuckdbFileHashRepository,
    DuckdbMetadataRepository, DuckdbVectorRepository, InMemoryVectorRepository, MockEmbedding,
    MockReranking, OrtEmbedding, OrtReranking, TreeSitterParser,
};

pub use domain::{
    compute_file_hash, CodeChunk, DomainError, Embedding, EmbeddingConfig, FileHash,
    IndexingStatus, Language, NodeType, ReferenceKind, Repository, SearchQuery, SearchResult,
    SymbolReference, VectorStore,
};

pub use connector::api::{Container, ContainerConfig, Router};
