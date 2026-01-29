//! # Connector Layer
//!
//! External integrations implementing domain interfaces:
//! - Embedding generation (mock for now, extensible for real models)
//! - Storage (SQLite for metadata, in-memory for embeddings)
//! - Parsing (Tree-sitter for AST)

pub mod embedding;
pub mod parser;
pub mod storage;

pub use embedding::*;
pub use parser::*;
pub use storage::*;
