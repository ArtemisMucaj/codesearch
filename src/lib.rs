pub mod application;
pub mod connector;
pub mod domain;

pub use application::{
    DeleteRepositoryUseCase, EmbeddingService, IndexRepositoryUseCase, ListRepositoriesUseCase,
    MetadataRepository, ParserService, RerankingService, SearchCodeUseCase, VectorRepository,
};

pub use connector::{
    ChromaVectorRepository, DuckdbMetadataRepository, DuckdbVectorRepository,
    InMemoryVectorRepository, MockEmbedding, MockReranking, OrtEmbedding, OrtReranking,
    TreeSitterParser,
};

pub use domain::{
    CodeChunk, DomainError, Embedding, EmbeddingConfig, IndexingStatus, Language, NodeType,
    Repository, SearchQuery, SearchResult, VectorStore,
};
