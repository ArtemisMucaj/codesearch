//! Repository traits defining interfaces for data persistence.

mod chunk_repository;
mod embedding_repository;
mod repository_repository;

pub use chunk_repository::*;
pub use embedding_repository::*;
pub use repository_repository::*;
