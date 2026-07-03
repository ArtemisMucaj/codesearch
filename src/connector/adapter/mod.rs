/// Sentinel stored as `embedding_model` / `embedding_target` in
/// `namespace_config` when a namespace is created with `--no-embeddings`.
/// Namespaces carrying this sentinel skip the embedding-space mismatch
/// validation — there are no vectors to protect.  Lives at the adapter
/// module root because it is shared configuration vocabulary, not a detail
/// of any single adapter.
pub const NO_EMBEDDINGS_MODEL: &str = "none";

/// Default ONNX embedding model used when a namespace is created (or first
/// indexed) without an explicit `--embedding-model`.
pub const DEFAULT_ONNX_EMBEDDING_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";

mod anthropic_client;
mod anthropic_reranking;
mod chat_client;
mod duckdb_call_graph_repository;
mod duckdb_channel_endpoint_repository;
mod duckdb_file_hash_repository;
mod duckdb_metadata_repository;
mod duckdb_vector_repository;
mod in_memory_vector_repository;
mod llm_query_expander;
pub mod mcp;
mod mock_embedding;
mod mock_reranking;
mod no_embedding;
mod openai_chat_client;
mod openai_embedding;
mod openai_reranking;
mod ort_embedding;
mod ort_reranking;
pub mod scip;
mod treesitter_parser;

pub use anthropic_client::*;
pub use anthropic_reranking::*;
pub use chat_client::*;
pub use duckdb_call_graph_repository::*;
pub use duckdb_channel_endpoint_repository::*;
pub use duckdb_file_hash_repository::*;
pub use duckdb_metadata_repository::*;
pub use duckdb_vector_repository::*;
pub use in_memory_vector_repository::*;
pub use llm_query_expander::*;
pub use mock_embedding::*;
pub use mock_reranking::*;
pub use no_embedding::*;
pub use openai_chat_client::*;
pub use openai_embedding::*;
pub use openai_reranking::*;
pub use ort_embedding::*;
pub use ort_reranking::*;
pub use treesitter_parser::*;
