use anyhow::Result;

use crate::cli::{
    ClaimsSubcommand, ClustersSubcommand, MemorySubcommand, SymbolClustersSubcommand,
};
use crate::{Commands, FeaturesSubcommand};

use super::container::Container;
use super::controller::{
    ChannelsController, ClaimsController, ClustersController, CouplingsController,
    DeleteController, ExecutionFeaturesController, ExplainController, ImpactController,
    IndexController, ListRepositoriesController, MemoryController, OverviewController,
    SearchController, StatsController, SymbolClustersController, SymbolContextController,
    UsesController, VisualizeController,
};

pub struct Router<'a> {
    channels_controller: ChannelsController<'a>,
    search_controller: SearchController<'a>,
    impact_controller: ImpactController<'a>,
    explain_controller: ExplainController<'a>,
    symbol_context_controller: SymbolContextController<'a>,
    stats_controller: StatsController<'a>,
    index_controller: IndexController<'a>,
    list_repositories_controller: ListRepositoriesController<'a>,
    memory_controller: MemoryController<'a>,
    claims_controller: ClaimsController<'a>,
    delete_controller: DeleteController<'a>,
    uses_controller: UsesController<'a>,
    execution_features_controller: ExecutionFeaturesController<'a>,
    clusters_controller: ClustersController<'a>,
    symbol_clusters_controller: SymbolClustersController<'a>,
    couplings_controller: CouplingsController<'a>,
    visualize_controller: VisualizeController<'a>,
    overview_controller: OverviewController<'a>,
}

impl<'a> Router<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self {
            channels_controller: ChannelsController::new(container),
            search_controller: SearchController::new(container),
            impact_controller: ImpactController::new(container),
            explain_controller: ExplainController::new(container),
            symbol_context_controller: SymbolContextController::new(container),
            stats_controller: StatsController::new(container),
            index_controller: IndexController::new(container),
            list_repositories_controller: ListRepositoriesController::new(container),
            memory_controller: MemoryController::new(container),
            claims_controller: ClaimsController::new(container),
            delete_controller: DeleteController::new(container),
            uses_controller: UsesController::new(container),
            execution_features_controller: ExecutionFeaturesController::new(container),
            clusters_controller: ClustersController::new(container),
            symbol_clusters_controller: SymbolClustersController::new(container),
            couplings_controller: CouplingsController::new(container),
            visualize_controller: VisualizeController::new(container),
            overview_controller: OverviewController::new(container),
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
                repository,
                format,
                regex,
            } => {
                self.impact_controller
                    .impact(symbol, repository, format, regex)
                    .await
            }
            Commands::Context {
                symbol,
                repository,
                format,
                regex,
            } => {
                self.symbol_context_controller
                    .context(symbol, repository, format, regex)
                    .await
            }
            Commands::Explain {
                symbol,
                repository,
                llm,
                dump_symbols,
                regex,
            } => {
                self.explain_controller
                    .explain(symbol, repository, llm, dump_symbols, regex)
                    .await
            }
            Commands::Features { subcommand } => match subcommand {
                FeaturesSubcommand::List {
                    repository,
                    limit,
                    format,
                } => {
                    self.execution_features_controller
                        .list(repository, limit, format)
                        .await
                }
                FeaturesSubcommand::Get {
                    symbol,
                    repository,
                    format,
                } => {
                    self.execution_features_controller
                        .get(symbol, repository, format)
                        .await
                }
                FeaturesSubcommand::Impacted {
                    symbols,
                    repository,
                    format,
                } => {
                    self.execution_features_controller
                        .impacted(symbols, repository, format)
                        .await
                }
            },
            Commands::Channels {
                repository,
                protocol,
                min_confidence,
                exclude_channel,
                include_tests,
                format,
            } => {
                self.channels_controller
                    .channels(
                        repository,
                        protocol,
                        min_confidence,
                        exclude_channel,
                        include_tests,
                        format,
                    )
                    .await
            }
            Commands::Uses { from, to } => self.uses_controller.uses(from, to).await,
            Commands::Overview {
                repository,
                format,
                top,
                skip,
                llm,
                no_llm,
            } => {
                self.overview_controller
                    .overview(repository, format, top, skip, llm, no_llm)
                    .await
            }
            Commands::Clusters { subcommand } => match subcommand {
                ClustersSubcommand::List {
                    repository,
                    format,
                    llm,
                    no_llm,
                } => {
                    self.clusters_controller
                        .list(repository, format, llm, no_llm)
                        .await
                }
                ClustersSubcommand::Get {
                    file,
                    repository,
                    format,
                } => self.clusters_controller.get(file, repository, format).await,
            },
            Commands::SymbolClusters { subcommand } => match subcommand {
                SymbolClustersSubcommand::List {
                    repository,
                    format,
                    llm,
                    no_llm,
                } => {
                    self.symbol_clusters_controller
                        .list(repository, format, llm, no_llm)
                        .await
                }
                SymbolClustersSubcommand::Get {
                    symbol,
                    repository,
                    format,
                } => {
                    self.symbol_clusters_controller
                        .get(symbol, repository, format)
                        .await
                }
            },
            Commands::Couplings {
                repository,
                level,
                format,
            } => {
                self.couplings_controller
                    .couplings(repository, level, format)
                    .await
            }
            Commands::Visualize {
                repository,
                level,
                format,
                output,
                aggregate,
                node_limit,
            } => {
                self.visualize_controller
                    .visualize(repository, level, format, output, aggregate, node_limit)
                    .await
            }
            Commands::Memory { subcommand } => match subcommand {
                MemorySubcommand::Import { path, llm, force } => match path {
                    Some(path) => self.memory_controller.import(path, llm, force).await,
                    // The no-path picker flow runs the TUI before the container
                    // is built, so it is handled in main.rs, not here.
                    None => Err(anyhow::anyhow!(
                        "interactive memory import is handled separately in main"
                    )),
                },
                MemorySubcommand::Search {
                    query,
                    num,
                    kind,
                    project,
                    all_projects,
                    format,
                } => {
                    self.memory_controller
                        .search(query, num, kind, project, all_projects, format)
                        .await
                }
                MemorySubcommand::List { kind, format } => {
                    self.memory_controller.list(kind, format).await
                }
                MemorySubcommand::Show { id } => self.memory_controller.show(id).await,
                MemorySubcommand::Delete { id } => self.memory_controller.delete(id).await,
                MemorySubcommand::Sessions { format } => {
                    self.memory_controller.sessions(format).await
                }
                MemorySubcommand::Add { source, name, llm } => {
                    self.memory_controller.add_resource(source, name, llm).await
                }
                MemorySubcommand::Tree { uri, format } => {
                    self.memory_controller.tree(uri, format).await
                }
                MemorySubcommand::Dream { llm, idle_minutes } => {
                    self.memory_controller.dream(llm, idle_minutes).await
                }
            },
            Commands::Claims { subcommand } => match subcommand {
                ClaimsSubcommand::Ingest { path, llm, force } => {
                    self.claims_controller.ingest(path, llm, force).await
                }
                ClaimsSubcommand::Recall {
                    query,
                    num,
                    project,
                    all_projects,
                    format,
                } => {
                    self.claims_controller
                        .recall(query, num, project, all_projects, format)
                        .await
                }
                ClaimsSubcommand::Dream { llm } => self.claims_controller.dream(llm).await,
                ClaimsSubcommand::Stats { format } => self.claims_controller.stats(format).await,
            },
            Commands::Create { .. } => Err(anyhow::anyhow!(
                "create command is handled separately in main"
            )),
            Commands::Mcp { .. } => {
                Err(anyhow::anyhow!("MCP command is handled separately in main"))
            }
            Commands::Tui { .. } => {
                Err(anyhow::anyhow!("TUI command is handled separately in main"))
            }
            Commands::Serve { .. } => Err(anyhow::anyhow!(
                "serve command is handled separately in main"
            )),
            Commands::Copilot { .. } => Err(anyhow::anyhow!(
                "copilot command is handled separately in main"
            )),
            Commands::Openai { .. } => Err(anyhow::anyhow!(
                "openai command is handled separately in main"
            )),
        }
    }
}
