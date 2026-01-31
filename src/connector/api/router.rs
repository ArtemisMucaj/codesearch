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
            Commands::Index { path, name } => self.repository_controller.index(path, name).await,
            Commands::Search {
                query,
                num,
                min_score,
                language,
                repository,
            } => {
                self.search_controller
                    .search(query, num, min_score, language, repository)
                    .await
            }
            Commands::List => self.repository_controller.list().await,
            Commands::Delete { id_or_path } => self.repository_controller.delete(id_or_path).await,
            Commands::Stats => self.search_controller.stats().await,
        }
    }
}
