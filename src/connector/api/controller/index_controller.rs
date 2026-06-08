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

        // Drop a per-project marker so the installed agent hooks know this
        // working tree is indexed and may nudge toward codesearch. Best-effort:
        // a marker-write failure must never fail the index.
        self.write_project_marker(&path, &repo).await;

        Ok(self.format_index_success(&repo))
    }

    /// Write `.codesearch/project.json` into the indexed repository root.
    /// Skipped for in-memory indexing (no persistent project to mark). The
    /// filesystem work runs on a blocking task so it never stalls a Tokio
    /// worker during concurrent indexing.
    async fn write_project_marker(&self, path: &str, repo: &Repository) {
        use crate::agent::marker::{write_marker, ProjectMarker};

        if self.container.memory_storage() {
            return;
        }
        let path = path.to_string();
        let marker = ProjectMarker::new(
            repo.id().to_string(),
            repo.name().to_string(),
            repo.namespace().map(str::to_string),
        );
        let result = tokio::task::spawn_blocking(move || {
            let root =
                std::fs::canonicalize(&path).unwrap_or_else(|_| std::path::PathBuf::from(&path));
            write_marker(&root, &marker).map(|_| root)
        })
        .await;
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("failed to write project marker: {}", e),
            Err(e) => tracing::warn!("project marker task failed to join: {}", e),
        }
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
