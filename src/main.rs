//! CodeSearch CLI - Semantic code search tool.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use codesearch::{
    DeleteRepositoryUseCase, IndexRepositoryUseCase, InMemoryEmbeddingStorage,
    ListRepositoriesUseCase, MockEmbeddingService, SearchCodeUseCase, SearchQuery, SqliteStorage,
    TreeSitterParser,
};

/// CodeSearch - Semantic code search powered by embeddings
#[derive(Parser)]
#[command(name = "codesearch")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Path to the data directory
    #[arg(short, long, global = true, default_value = "~/.codesearch")]
    data_dir: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index a code repository
    Index {
        /// Path to the repository to index
        path: String,

        /// Optional name for the repository
        #[arg(short, long)]
        name: Option<String>,
    },

    /// Search for code
    Search {
        /// The search query
        query: String,

        /// Maximum number of results
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Minimum similarity score (0.0 to 1.0)
        #[arg(short, long)]
        min_score: Option<f32>,

        /// Filter by language
        #[arg(short = 'L', long)]
        language: Option<Vec<String>>,

        /// Filter by repository ID
        #[arg(short, long)]
        repository: Option<Vec<String>>,
    },

    /// List indexed repositories
    List,

    /// Delete an indexed repository
    Delete {
        /// Repository ID or path to delete
        id_or_path: String,
    },

    /// Show statistics
    Stats,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Setup logging
    let level = if cli.verbose { Level::DEBUG } else { Level::INFO };
    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Expand data directory path
    let data_dir = expand_tilde(&cli.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    // Initialize storage
    let db_path = PathBuf::from(&data_dir).join("codesearch.db");
    let sqlite = Arc::new(SqliteStorage::new(&db_path)?);

    // Initialize services
    let parser = Arc::new(TreeSitterParser::new());
    let embedding_service = Arc::new(MockEmbeddingService::new());
    let embedding_repo = Arc::new(InMemoryEmbeddingStorage::new(sqlite.clone()));

    match cli.command {
        Commands::Index { path, name } => {
            info!("Initializing embedding service...");

            let use_case = IndexRepositoryUseCase::new(
                sqlite.clone(),
                sqlite.clone(),
                embedding_repo,
                parser,
                embedding_service,
            );

            let repo = use_case.execute(&path, name.as_deref()).await?;
            println!(
                "Successfully indexed repository: {} ({} files, {} chunks)",
                repo.name, repo.file_count, repo.chunk_count
            );
        }

        Commands::Search {
            query,
            limit,
            min_score,
            language,
            repository,
        } => {
            let use_case = SearchCodeUseCase::new(embedding_repo, embedding_service);

            let mut search_query = SearchQuery::new(&query).with_limit(limit);

            if let Some(score) = min_score {
                search_query = search_query.with_min_score(score);
            }

            if let Some(langs) = language {
                search_query = search_query.with_languages(langs);
            }

            if let Some(repos) = repository {
                search_query = search_query.with_repositories(repos);
            }

            let results = use_case.execute(search_query).await?;

            if results.is_empty() {
                println!("No results found.");
            } else {
                println!("Found {} results:\n", results.len());

                for (i, result) in results.iter().enumerate() {
                    println!(
                        "{}. {} (score: {:.3})",
                        i + 1,
                        result.chunk.location(),
                        result.score
                    );

                    if let Some(ref name) = result.chunk.symbol_name {
                        println!("   Symbol: {} ({})", name, result.chunk.node_type);
                    }

                    let preview: String = result
                        .chunk
                        .content
                        .lines()
                        .take(3)
                        .map(|l| format!("   | {}", l))
                        .collect::<Vec<_>>()
                        .join("\n");
                    println!("{}", preview);
                    println!();
                }
            }
        }

        Commands::List => {
            let use_case = ListRepositoriesUseCase::new(sqlite.clone());
            let repos = use_case.execute().await?;

            if repos.is_empty() {
                println!("No repositories indexed.");
            } else {
                println!("Indexed repositories:\n");
                for repo in repos {
                    println!("  {} ({})", repo.name, repo.id);
                    println!("    Path: {}", repo.path);
                    println!("    Files: {}, Chunks: {}", repo.file_count, repo.chunk_count);
                    println!();
                }
            }
        }

        Commands::Delete { id_or_path } => {
            let use_case =
                DeleteRepositoryUseCase::new(sqlite.clone(), sqlite.clone(), embedding_repo);

            let result = use_case.execute(&id_or_path).await;
            match result {
                Ok(_) => println!("Repository deleted successfully."),
                Err(_) => {
                    use_case.delete_by_path(&id_or_path).await?;
                    println!("Repository deleted successfully.");
                }
            }
        }

        Commands::Stats => {
            let list_use_case = ListRepositoriesUseCase::new(sqlite.clone());
            let repos = list_use_case.execute().await?;

            let total_repos = repos.len();
            let total_files: u64 = repos.iter().map(|r| r.file_count).sum();
            let total_chunks: u64 = repos.iter().map(|r| r.chunk_count).sum();

            println!("CodeSearch Statistics");
            println!("=====================");
            println!("Repositories: {}", total_repos);
            println!("Total Files:  {}", total_files);
            println!("Total Chunks: {}", total_chunks);
            println!("Data Dir:     {}", data_dir);
        }
    }

    Ok(())
}

/// Expand ~ to home directory.
fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return path.replacen("~", &home.to_string_lossy(), 1);
        }
    }
    path.to_string()
}
