use anyhow::Result;

use crate::{Repository, SearchQuery, SearchResult};

use super::super::Container;

pub struct SearchController<'a> {
    container: &'a Container,
}

impl<'a> SearchController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn search(
        &self,
        query: String,
        num: usize,
        min_score: Option<f32>,
        languages: Option<Vec<String>>,
        repositories: Option<Vec<String>>,
    ) -> Result<String> {
        let mut search_query = SearchQuery::new(&query).with_limit(num);

        if let Some(score) = min_score {
            search_query = search_query.with_min_score(score);
        }

        if let Some(langs) = languages {
            search_query = search_query.with_languages(langs);
        }

        if let Some(repos) = repositories {
            search_query = search_query.with_repositories(repos);
        }

        let use_case = self.container.search_use_case();
        let results = use_case.execute(search_query).await?;

        Ok(self.format_search_results(&results))
    }

    pub async fn stats(&self) -> Result<String> {
        let use_case = self.container.list_use_case();
        let repos = use_case.execute().await?;

        Ok(self.format_stats(&repos))
    }

    fn format_search_results(&self, results: &[SearchResult]) -> String {
        if results.is_empty() {
            return "No results found.".to_string();
        }

        let mut output = format!("Found {} results:\n\n", results.len());

        for (i, result) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {} (score: {:.3})\n",
                i + 1,
                result.chunk().location(),
                result.score()
            ));

            if let Some(name) = result.chunk().symbol_name() {
                output.push_str(&format!(
                    "   Symbol: {} ({})\n",
                    name,
                    result.chunk().node_type()
                ));
            }

            let preview: String = result
                .chunk()
                .content()
                .lines()
                .take(10)
                .map(|l| format!("   | {}", l))
                .collect::<Vec<_>>()
                .join("\n");
            output.push_str(&preview);
            output.push_str("\n\n");
        }

        output
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
