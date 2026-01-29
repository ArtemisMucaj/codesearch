//! # CodeSearch
//!
//! A semantic code search tool that indexes code repositories using embeddings
//! and AST analysis for intelligent code discovery.
//!
//! ## Architecture
//!
//! The crate is organized following Domain-Driven Design principles:
//!
//! - `domain`: Core business models, repository traits, and service interfaces
//! - `application`: Use cases and orchestration logic
//! - `connector`: External integrations (SQLite, Tree-sitter, embeddings)

pub mod application;
pub mod connector;
pub mod domain;

// Re-export commonly used types
pub use application::*;
pub use connector::*;
pub use domain::*;
