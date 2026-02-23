use async_trait::async_trait;

use crate::domain::DomainError;

/// An interface for sending chat-style prompts to an LLM and receiving text responses.
///
/// Implementors encapsulate transport, serialization, and vendor-specific API
/// details.  Consumers (e.g. [`super::LlmQueryExpander`]) remain decoupled from
/// any particular provider or HTTP client library.
#[async_trait]
pub trait ChatClient: Send + Sync {
    /// Send a `system` context message followed by a `user` prompt and return
    /// the assistant's response text.
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError>;
}
