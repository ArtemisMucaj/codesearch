use anyhow::Result;

use crate::Commands;

use super::container::Container;
use super::controller::{RepositoryController, SearchController};

pub struct Router<'a> {
    repository_controller: RepositoryController<'a>,
    search_controller: SearchController<'a>,
}

impl<'a> Router<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self {
            repository_controller: RepositoryController::new(container),
            search_controller: SearchController::new(container),
        }
    }

    pub async fn route(&self, command: Commands) -> Result<String> {
        match command {
            Commands::Index { path, name, force } => {
                self.repository_controller.index(path, name, force).await
            }
            Commands::Search {
                query,
                num,
                min_score,
                language,
                repository,
                format,
                no_text_search,
            } => {
                self.search_controller
                    .search(query, num, min_score, language, repository, format, !no_text_search)
                    .await
            }
            Commands::List => self.repository_controller.list().await,
            Commands::Delete { id_or_path } => self.repository_controller.delete(id_or_path).await,
            Commands::Stats => self.search_controller.stats().await,
            Commands::Impact {
                symbol,
                depth,
                repository,
                format,
            } => {
                self.search_controller
                    .impact(symbol, depth, repository, format)
                    .await
            }
            Commands::Context {
                symbol,
                repository,
                limit,
                format,
            } => {
                self.search_controller
                    .context(symbol, repository, limit, format)
                    .await
            }
            Commands::Mcp { .. } => unreachable!("MCP command is handled separately in main"),
        }
    }
}
