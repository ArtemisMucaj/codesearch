use anyhow::Result;

use crate::{Repository, VectorStore};

use super::super::Container;

pub struct IndexController<'a> {
    container: &'a Container,
}

impl<'a> IndexController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn index(&self, path: String, name: Option<String>, force: bool) -> Result<String> {
        let (vector_store, ns): (VectorStore, Option<String>) = if self.container.memory_storage() {
            (VectorStore::InMemory, None)
        } else if self.container.chroma_url().is_some() {
            (
                VectorStore::ChromaDb,
                Some(self.container.namespace().to_string()),
            )
        } else {
            (
                VectorStore::DuckDb,
                Some(self.container.namespace().to_string()),
            )
        };

        let use_case = self.container.index_use_case();
        let repo = use_case
            .execute(&path, name.as_deref(), vector_store, ns, force)
            .await?;

        Ok(self.format_index_success(&repo))
    }

    fn format_index_success(&self, repo: &Repository) -> String {
        let mut output = format!(
            "Successfully indexed repository: {} ({} files, {} chunks)",
            repo.name(),
            repo.file_count(),
            repo.chunk_count()
        );

        if !repo.languages().is_empty() {
            let langs: Vec<_> = repo
                .languages()
                .iter()
                .map(|(lang, stats)| format!("{}: {} files", lang, stats.file_count))
                .collect();
            output.push_str(&format!("\nLanguages: {}", langs.join(", ")));
        }

        output
    }
}
