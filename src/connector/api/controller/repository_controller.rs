use anyhow::Result;

use crate::{DomainError, Repository};

use super::super::Container;

pub struct RepositoryController<'a> {
    container: &'a Container,
}

impl<'a> RepositoryController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn index(&self, path: String, name: Option<String>) -> Result<String> {
        use crate::VectorStore;

        let (vector_store, ns): (VectorStore, Option<String>) = if self.container.memory_storage() {
            (VectorStore::InMemory, None)
        } else if self.container.chroma_url().is_some() {
            (VectorStore::ChromaDb, Some(self.container.namespace().to_string()))
        } else {
            (VectorStore::DuckDb, Some(self.container.namespace().to_string()))
        };

        let use_case = self.container.index_use_case();
        let repo = use_case.execute(&path, name.as_deref(), vector_store, ns).await?;

        Ok(self.format_index_success(&repo))
    }

    pub async fn list(&self) -> Result<String> {
        let use_case = self.container.list_use_case();
        let repos = use_case.execute().await?;

        Ok(self.format_repository_list(&repos))
    }

    pub async fn delete(&self, id_or_path: String) -> Result<String> {
        let use_case = self.container.delete_use_case();

        match use_case.execute(&id_or_path).await {
            Ok(_) => Ok(self.format_delete_success()),
            Err(e) => {
                // Only try path-based deletion if the ID was not found
                if matches!(e, DomainError::NotFound(_)) {
                    use_case.delete_by_path(&id_or_path).await?;
                    Ok(self.format_delete_success())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    fn format_index_success(&self, repo: &Repository) -> String {
        format!(
            "Successfully indexed repository: {} ({} files, {} chunks)",
            repo.name(),
            repo.file_count(),
            repo.chunk_count()
        )
    }

    fn format_repository_list(&self, repos: &[Repository]) -> String {
        if repos.is_empty() {
            return "No repositories indexed.".to_string();
        }

        let mut output = "Indexed repositories:\n\n".to_string();
        for repo in repos {
            output.push_str(&format!("  {} ({})\n", repo.name(), repo.id()));
            output.push_str(&format!("    Path: {}\n", repo.path()));
            output.push_str(&format!(
                "    Files: {}, Chunks: {}\n",
                repo.file_count(),
                repo.chunk_count()
            ));
            let ns_display = repo.namespace().unwrap_or("(none)");
            output.push_str(&format!(
                "    Store: {}, Namespace: {}\n",
                repo.store().as_str(),
                ns_display
            ));
            output.push_str("\n");
        }

        output
    }

    fn format_delete_success(&self) -> String {
        "Repository deleted successfully.".to_string()
    }
}
