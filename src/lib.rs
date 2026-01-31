pub mod application;
pub mod cli;
pub mod connector;
pub mod domain;

pub use application::{
    DeleteRepositoryUseCase, EmbeddingService, IndexRepositoryUseCase, ListRepositoriesUseCase,
    MetadataRepository, ParserService, RerankingService, SearchCodeUseCase, VectorRepository,
};

pub use cli::Commands;

pub use connector::{
    ChromaVectorRepository, DuckdbMetadataRepository, DuckdbVectorRepository,
    InMemoryVectorRepository, MockEmbedding, MockReranking, OrtEmbedding, OrtReranking,
    TreeSitterParser,
};

pub use domain::{
    CodeChunk, DomainError, Embedding, EmbeddingConfig, IndexingStatus, Language, NodeType,
    Repository, SearchQuery, SearchResult, VectorStore,
};

pub use connector::api::{Container, ContainerConfig, Router};
