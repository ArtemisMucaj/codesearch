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

    /// Like [`Self::complete`], but constrains the response to conform to the
    /// given JSON Schema. Backends that support structured/grammar-constrained
    /// decoding (e.g. an OpenAI-compatible server's `response_format`) return
    /// JSON that is guaranteed to match the schema, which is far more robust
    /// than parsing free-form output — especially with small local models.
    ///
    /// `schema_name` is a short identifier for the schema; `schema` is the JSON
    /// Schema object. The default implementation ignores the schema and falls
    /// back to [`Self::complete`], so providers without structured output still
    /// satisfy the contract (the caller must then tolerate best-effort JSON).
    async fn complete_json(
        &self,
        system: &str,
        user: &str,
        _schema_name: &str,
        _schema: &serde_json::Value,
    ) -> Result<String, DomainError> {
        self.complete(system, user).await
    }

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
