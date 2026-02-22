use anyhow::Result;

use crate::Repository;

use super::super::Container;

pub struct StatsController<'a> {
    container: &'a Container,
}

impl<'a> StatsController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn stats(&self) -> Result<String> {
        let use_case = self.container.list_use_case();
        let repos = use_case.execute().await?;
        Ok(self.format_stats(&repos))
    }

    fn format_stats(&self, repos: &[Repository]) -> String {
        let total_repos = repos.len();
        let total_files: u64 = repos.iter().map(|r| r.file_count()).sum();
        let total_chunks: u64 = repos.iter().map(|r| r.chunk_count()).sum();

        format!(
            "CodeSearch Statistics\n=====================\nRepositories: {}\nTotal Files:  {}\nTotal Chunks: {}\nData Dir:     {}",
            total_repos,
            total_files,
            total_chunks,
            self.container.data_dir()
        )
    }
}
