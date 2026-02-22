use anyhow::Result;

use crate::Repository;

use super::super::Container;

pub struct ListRepositoriesController<'a> {
    container: &'a Container,
}

impl<'a> ListRepositoriesController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn list(&self) -> Result<String> {
        let use_case = self.container.list_use_case();
        let repos = use_case.execute().await?;
        Ok(self.format_repository_list(&repos))
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
            if !repo.languages().is_empty() {
                let langs: Vec<_> = repo
                    .languages()
                    .iter()
                    .map(|(lang, stats)| format!("{}: {} files", lang, stats.file_count))
                    .collect();
                output.push_str(&format!("    Languages: {}\n", langs.join(", ")));
            }
            let ns_display = repo.namespace().unwrap_or("(none)");
            output.push_str(&format!(
                "    Store: {}, Namespace: {}\n",
                repo.store().as_str(),
                ns_display
            ));
            output.push('\n');
        }

        output
    }
}
