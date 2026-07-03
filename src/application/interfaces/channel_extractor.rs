use async_trait::async_trait;

use crate::domain::{ChannelEndpoint, DomainError, Language};

/// Extraction of communication endpoints (producer/consumer call sites with
/// their channel identifier) from source code.
///
/// Implementations live in the connector layer (tree-sitter detector
/// registry). Extraction runs during indexing, right after chunk parsing, and
/// must be deterministic: the same content always yields the same endpoints
/// (modulo generated ids).
#[async_trait]
pub trait ChannelExtractor: Send + Sync {
    /// Extract every channel endpoint found in `content`.
    ///
    /// Call sites whose channel argument is an identifier rather than a
    /// literal are still returned — marked unresolved — so they appear in the
    /// unmatched report and can be resolved by later passes.
    async fn extract(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
    ) -> Result<Vec<ChannelEndpoint>, DomainError>;

    /// True when at least one detector is registered for `language`.
    fn supports_language(&self, language: Language) -> bool;
}
