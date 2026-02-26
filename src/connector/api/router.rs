use anyhow::Result;

use crate::Commands;

use super::container::Container;
use super::controller::{
    DeleteController, ImpactController, IndexController, ListRepositoriesController,
    SearchController, StatsController, SymbolContextController,
};

pub struct Router<'a> {
    search_controller: SearchController<'a>,
    impact_controller: ImpactController<'a>,
    symbol_context_controller: SymbolContextController<'a>,
    stats_controller: StatsController<'a>,
    index_controller: IndexController<'a>,
    list_repositories_controller: ListRepositoriesController<'a>,
    delete_controller: DeleteController<'a>,
}

impl<'a> Router<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self {
            search_controller: SearchController::new(container),
            impact_controller: ImpactController::new(container),
            symbol_context_controller: SymbolContextController::new(container),
            stats_controller: StatsController::new(container),
            index_controller: IndexController::new(container),
            list_repositories_controller: ListRepositoriesController::new(container),
            delete_controller: DeleteController::new(container),
        }
    }

    pub async fn route(&self, command: Commands) -> Result<String> {
        match command {
            Commands::Index { path, name, force } => {
                self.index_controller.index(path, name, force).await
            }
            Commands::Search {
                query,
                num,
                min_score,
                language,
                repository,
                format,
                text_search,
            } => {
                self.search_controller
                    .search(
                        query,
                        num,
                        min_score,
                        language,
                        repository,
                        format,
                        text_search,
                    )
                    .await
            }
            Commands::List => self.list_repositories_controller.list().await,
            Commands::Delete { id_or_path } => self.delete_controller.delete(id_or_path).await,
            Commands::Stats => self.stats_controller.stats().await,
            Commands::Impact {
                symbol,
                depth,
                repository,
                format,
            } => {
                self.impact_controller
                    .impact(symbol, depth, repository, format)
                    .await
            }
            Commands::Context {
                symbol,
                repository,
                limit,
                format,
            } => {
                self.symbol_context_controller
                    .context(symbol, repository, limit, format)
                    .await
            }
            Commands::Mcp { .. } => unreachable!("MCP command is handled separately in main"),
        }
    }
}
