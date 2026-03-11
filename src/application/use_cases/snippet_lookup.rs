use std::sync::Arc;

use crate::application::VectorRepository;
use crate::domain::{CodeChunk, DomainError};

/// Extract the short (unqualified) name from a fully-qualified symbol.
///
/// Handles the SCIP/tree-sitter FQN conventions used in the call graph:
/// - `Namespace\Class#method`  → `method`
/// - `Namespace\Class`         → `Class`
/// - `crate::module::fn`       → `fn`
fn short_symbol_name(symbol: &str) -> &str {
    // SCIP method notation: take everything after the last `#`
    if let Some(pos) = symbol.rfind('#') {
        return &symbol[pos + 1..];
    }
    // PHP/namespace backslash separator
    if let Some(pos) = symbol.rfind('\\') {
        return &symbol[pos + 1..];
    }
    // Rust/Go double-colon separator
    if let Some(pos) = symbol.rfind("::") {
        return &symbol[pos + 2..];
    }
    // Dot separator (Java, Python, JS)
    if let Some(pos) = symbol.rfind('.') {
        return &symbol[pos + 1..];
    }
    symbol
}

/// Extract a class/file hint from a fully-qualified symbol for disambiguation.
///
/// For `Namespace\Class#method` the class name is `Class`; chunks are typically
/// stored in a file whose name contains `Class`, so this hint lets the query
/// prefer the right file when the short method name is ambiguous.
///
/// Returns `None` when no useful class hint can be derived (e.g. bare symbol).
fn class_hint_from_symbol(symbol: &str) -> Option<&str> {
    // Only meaningful when there is a `#` — the class name is the segment
    // immediately before the `#`, after the last namespace separator.
    let class_and_method = symbol.rfind('#')?;
    let class_part = &symbol[..class_and_method];
    // Strip any namespace prefix (backslash, double-colon, or dot).
    let start = class_part
        .rfind('\\')
        .or_else(|| class_part.rfind("::").map(|p| p + 1))
        .or_else(|| class_part.rfind('.'))
        .map(|p| p + 1)
        .unwrap_or(0);
    let hint = &class_part[start..];
    if hint.is_empty() {
        None
    } else {
        Some(hint)
    }
}

/// Retrieves an indexed [`CodeChunk`] for a reference location shown in the TUI.
///
/// Given a file path and a line number (as returned by [`ContextNode`] or
/// [`ImpactNode`]), this use case queries the vector store for the chunks that
/// belong to that file and returns the smallest chunk whose line range contains
/// the reference line. Code is therefore always sourced from the indexed store,
/// never from the live filesystem.
pub struct SnippetLookupUseCase {
    vector_repo: Arc<dyn VectorRepository>,
}

impl SnippetLookupUseCase {
    pub fn new(vector_repo: Arc<dyn VectorRepository>) -> Self {
        Self { vector_repo }
    }

    /// Return the content of the indexed chunk that contains `line` in `file_path`.
    ///
    /// `repository_id` may be an empty string to search across all repositories.
    /// Returns `None` when no matching chunk is found (e.g. file not indexed).
    pub async fn get_snippet(
        &self,
        repository_id: &str,
        file_path: &str,
        line: u32,
    ) -> Result<Option<CodeChunk>, DomainError> {
        let chunks = self
            .vector_repo
            .find_chunks_by_file(repository_id, file_path)
            .await
            .map_err(|e| {
                DomainError::storage(format!(
                    "snippet lookup for '{file_path}' in repository '{repository_id}': {e}"
                ))
            })?;

        // Prefer the smallest chunk whose range fully contains the reference line
        // so we show the tightest relevant context (e.g. a function rather than a module).
        let best = chunks
            .iter()
            .filter(|c| c.start_line() <= line && c.end_line() >= line)
            .min_by_key(|c| c.end_line().saturating_sub(c.start_line()));

        Ok(best.cloned())
    }

    /// Return the definition chunk for a callee symbol given its fully-qualified name.
    ///
    /// Used for callee nodes in the Context tree view where only the callee FQN is
    /// known — the stored `file_path`/`line` on a callee `ContextNode` point to the
    /// call-site inside the root symbol, not the callee's own definition.
    ///
    /// Resolution strategy:
    /// 1. Extract the short name (`Class#method` → `method`).
    /// 2. Extract a class hint (`Namespace\Class#method` → `Class`) for
    ///    disambiguation when multiple symbols share the same short name.
    /// 3. Query chunks by short name; rank matches whose file path contains the
    ///    class hint higher, then prefer the smallest (tightest) chunk.
    pub async fn get_snippet_for_symbol(
        &self,
        repository_id: &str,
        symbol: &str,
    ) -> Result<Option<CodeChunk>, DomainError> {
        let short = short_symbol_name(symbol);
        let class_hint = class_hint_from_symbol(symbol);
        self.vector_repo
            .find_chunk_by_symbol(repository_id, short, class_hint)
            .await
            .map_err(|e| {
                DomainError::storage(format!(
                    "symbol snippet lookup for '{symbol}' (short: '{short}', \
                     class hint: {class_hint:?}) in repository '{repository_id}': {e}"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_symbol_name() {
        assert_eq!(short_symbol_name("Namespace\\Class#method"), "method");
        assert_eq!(short_symbol_name("Namespace\\Class"), "Class");
        assert_eq!(short_symbol_name("crate::module::func"), "func");
        assert_eq!(short_symbol_name("com.example.Foo"), "Foo");
        assert_eq!(short_symbol_name("bare"), "bare");
    }

    #[test]
    fn test_class_hint_from_symbol() {
        assert_eq!(
            class_hint_from_symbol("Namespace\\Class#method"),
            Some("Class")
        );
        assert_eq!(
            class_hint_from_symbol("GenericUtils#getIp"),
            Some("GenericUtils")
        );
        assert_eq!(class_hint_from_symbol("bare_function"), None);
        assert_eq!(class_hint_from_symbol("Namespace\\Class"), None);
    }
}
