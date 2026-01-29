pub mod application;
pub mod connector;
pub mod domain;

pub use application::{
    EmbeddingService, ParserService, RepositoryRepository, VectorRepository,
    DeleteRepositoryUseCase, IndexRepositoryUseCase, ListRepositoriesUseCase, SearchCodeUseCase,
};

pub use connector::{
    ChromaVectorRepository, InMemoryVectorRepository, MockEmbedding, 
    OrtEmbedding, SqliteRepositoryAdapter, TreeSitterParser,
};

pub use domain::{
    CodeChunk, DomainError, Embedding, EmbeddingConfig, IndexingStatus, 
    Language, NodeType, Repository, SearchQuery, SearchResult,
};
