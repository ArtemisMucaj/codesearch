use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use tracing::warn;

use crate::application::{CallGraphExtractor, ParserService};
use crate::domain::{DomainError, Language, SymbolReference};

/// Maximum number of files read concurrently during the export pre-scan.
const PRE_SCAN_CONCURRENCY: usize = 16;

/// Concrete [`CallGraphExtractor`] implementation backed by a [`ParserService`].
///
/// This is the default extractor used in production.  It delegates reference
/// extraction to the parser service and implements the JS/TS export pre-scan
/// by reading files from disk in parallel.
pub struct ParserBasedExtractor {
    parser_service: Arc<dyn ParserService>,
}

impl ParserBasedExtractor {
    pub fn new(parser_service: Arc<dyn ParserService>) -> Self {
        Self { parser_service }
    }
}

#[async_trait::async_trait]
impl CallGraphExtractor for ParserBasedExtractor {
    async fn extract(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
        exports_by_file: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        self.parser_service
            .extract_references(content, file_path, language, repository_id, exports_by_file)
            .await
    }

    async fn build_export_index(
        &self,
        absolute_path: &Path,
        relative_paths: &[String],
    ) -> HashMap<String, Vec<String>> {
        let parser = self.parser_service.clone();
        let absolute_path = absolute_path.to_path_buf();

        stream::iter(relative_paths.iter().cloned())
            .map(|rel_path| {
                let parser = parser.clone();
                let abs_path = absolute_path.clone();
                async move {
                    let lang = Language::from_path(Path::new(&rel_path));
                    if !matches!(lang, Language::JavaScript | Language::TypeScript) {
                        return None;
                    }
                    let content = match tokio::fs::read_to_string(abs_path.join(&rel_path)).await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(
                                "Failed to read file for export pre-scan {}: {}",
                                rel_path, e
                            );
                            return None;
                        }
                    };
                    let exports = parser.extract_module_exports(&content, lang).await;
                    if exports.is_empty() {
                        None
                    } else {
                        Some((rel_path, exports))
                    }
                }
            })
            .buffer_unordered(PRE_SCAN_CONCURRENCY)
            .filter_map(|x| async { x })
            .collect::<HashMap<String, Vec<String>>>()
            .await
    }
}
