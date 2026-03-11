use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;

use crate::domain::DomainError;

/// Port trait for sending chat-style prompts to an LLM and receiving text responses.
///
/// Implementors (in the connector layer) encapsulate transport, serialization,
/// and vendor-specific API details. Application-layer consumers remain decoupled
/// from any particular provider or HTTP client library.
#[async_trait]
pub trait ChatClient: Send + Sync {
    /// Send a `system` context message followed by a `user` prompt and return
    /// the assistant's response text.
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError>;

    /// Stream the assistant's response token by token.
    ///
    /// Each token chunk is sent to `token_tx` as it arrives.  The method
    /// returns the full concatenated text once the stream is exhausted.
    ///
    /// The default implementation calls [`Self::complete`] and delivers the
    /// entire response as a single chunk, so providers that do not support
    /// streaming still satisfy the contract.
    async fn complete_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        let result = self.complete(system, user).await?;
        let _ = token_tx.send(result.clone());
        Ok(result)
    }
}
