pub mod application;
pub mod connector;
pub mod domain;

pub use application::{
    EmbeddingService, ParserService, MetadataRepository, VectorRepository,
    DeleteRepositoryUseCase, IndexRepositoryUseCase, ListRepositoriesUseCase, SearchCodeUseCase,
};

pub use connector::{
    ChromaVectorRepository, DuckdbMetadataRepository, DuckdbVectorRepository, InMemoryVectorRepository,
    MockEmbedding, OrtEmbedding, TreeSitterParser,
};

pub use domain::{
    CodeChunk, DomainError, Embedding, EmbeddingConfig, IndexingStatus, 
    Language, NodeType, Repository, SearchQuery, SearchResult, VectorStore,
};
