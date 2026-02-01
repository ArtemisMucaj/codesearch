pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;

pub use application::{
    DeleteRepositoryUseCase, EmbeddingService, FileHashRepository, IndexRepositoryUseCase,
    ListRepositoriesUseCase, MetadataRepository, ParserService, RerankingService,
    SearchCodeUseCase, VectorRepository,
};

pub use cli::Commands;

pub use connector::{
    CandleEmbedding, CandleReranking, ChromaVectorRepository, DuckdbFileHashRepository,
    DuckdbMetadataRepository, DuckdbVectorRepository, InMemoryVectorRepository, MockEmbedding,
    MockReranking, TreeSitterParser,
};

pub use domain::{
    compute_file_hash, CodeChunk, DomainError, Embedding, EmbeddingConfig, FileHash,
    IndexingStatus, Language, NodeType, Repository, SearchQuery, SearchResult, VectorStore,
};

pub use connector::api::{Container, ContainerConfig, Router};
