use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use codesearch::{
    ChromaVectorRepository, DeleteRepositoryUseCase, EmbeddingService, IndexRepositoryUseCase,
    InMemoryVectorRepository, LanceDbVectorRepository, ListRepositoriesUseCase, MockEmbedding,
    OrtEmbedding, SearchCodeUseCase, SearchQuery, SqliteRepositoryAdapter, TreeSitterParser,
    VectorRepository,
};

#[derive(Parser)]
#[command(name = "codesearch")]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long, global = true)]
    verbose: bool,

    #[arg(short, long, global = true, default_value = "~/.codesearch")]
    data_dir: String,

    #[arg(long, global = true)]
    mock_embeddings: bool,

    #[arg(long, global = true)]
    model: Option<String>,

    #[arg(long, global = true, default_value = "http://localhost:8000")]
    chroma_url: String,

    #[arg(long, global = true, default_value = "codesearch")]
    chroma_collection: String,

    #[arg(long, global = true)]
    memory_storage: bool,

    #[arg(long, global = true, default_value = "lancedb", value_parser = ["lancedb", "chromadb"])]
    vector_backend: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Index {
        path: String,

        #[arg(short, long)]
        name: Option<String>,
    },

    Search {
        query: String,

        #[arg(short, long, default_value = "10")]
        limit: usize,

        #[arg(short, long)]
        min_score: Option<f32>,

        #[arg(short = 'L', long)]
        language: Option<Vec<String>>,

        #[arg(short, long)]
        repository: Option<Vec<String>>,
    },

    List,

    Delete {
        id_or_path: String,
    },

    Stats,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Configure logging with proper filters
    // By default: only show warnings from dependencies, info from codesearch
    // With -v: debug from codesearch, warnings from dependencies
    let env_filter = if cli.verbose {
        EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new("codesearch=debug,warn"))
            .unwrap_or_else(|_| EnvFilter::new("warn"))
    } else {
        EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new("codesearch=info,warn"))
            .unwrap_or_else(|_| EnvFilter::new("warn"))
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_target(false))
        .init();

    let data_dir_str = expand_tilde(&cli.data_dir);
    let data_dir = PathBuf::from(&data_dir_str);
    std::fs::create_dir_all(&data_dir)?;

    let db_path = data_dir.join("codesearch.db");
    let sqlite = Arc::new(SqliteRepositoryAdapter::new(&db_path)?);

    let parser = Arc::new(TreeSitterParser::new());
    let embedding_service: Arc<dyn EmbeddingService> = if cli.mock_embeddings {
        info!("Using mock embedding service");
        Arc::new(MockEmbedding::new())
    } else {
        info!("Initializing ONNX embedding service...");
        Arc::new(OrtEmbedding::new(cli.model.as_deref())?)
    };

    let vector_repo: Arc<dyn VectorRepository> = if cli.memory_storage {
        info!("Using in-memory vector storage");
        Arc::new(InMemoryVectorRepository::new())
    } else {
        match cli.vector_backend.as_str() {
            "lancedb" => {
                match LanceDbVectorRepository::new(&data_dir, "code_chunks").await {
                    Ok(lancedb) => {
                        info!("Initialized LanceDB vector storage at {}/lancedb", data_dir_str);
                        Arc::new(lancedb)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to initialize LanceDB: {}. Falling back to in-memory storage.",
                            e
                        );
                        Arc::new(InMemoryVectorRepository::new())
                    }
                }
            }
            "chromadb" => {
                match ChromaVectorRepository::new(&cli.chroma_url, &cli.chroma_collection).await {
                    Ok(chroma) => {
                        info!("Connected to ChromaDB at {}", cli.chroma_url);
                        Arc::new(chroma)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to connect to ChromaDB ({}): {}. Falling back to in-memory storage.",
                            cli.chroma_url,
                            e
                        );
                        Arc::new(InMemoryVectorRepository::new())
                    }
                }
            }
            _ => {
                tracing::warn!("Invalid vector backend: {}. Using in-memory storage.", cli.vector_backend);
                Arc::new(InMemoryVectorRepository::new())
            }
        }
    };

    match cli.command {
        Commands::Index { path, name } => {
            let use_case = IndexRepositoryUseCase::new(
                sqlite.clone(),
                vector_repo,
                parser,
                embedding_service,
            );

            let repo = use_case.execute(&path, name.as_deref()).await?;
            println!(
                "Successfully indexed repository: {} ({} files, {} chunks)",
                repo.name(), repo.file_count(), repo.chunk_count()
            );
        }

        Commands::Search {
            query,
            limit,
            min_score,
            language,
            repository,
        } => {
            let use_case = SearchCodeUseCase::new(vector_repo, embedding_service);

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
                        result.chunk().location(),
                        result.score()
                    );

                    if let Some(name) = result.chunk().symbol_name() {
                        println!("   Symbol: {} ({})", name, result.chunk().node_type());
                    }

                    let preview: String = result
                        .chunk()
                        .content()
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
                    println!("  {} ({})", repo.name(), repo.id());
                    println!("    Path: {}", repo.path());
                    println!("    Files: {}, Chunks: {}", repo.file_count(), repo.chunk_count());
                    println!();
                }
            }
        }

        Commands::Delete { id_or_path } => {
            let use_case = DeleteRepositoryUseCase::new(sqlite.clone(), vector_repo);

            match use_case.execute(&id_or_path).await {
                Ok(_) => println!("Repository deleted successfully."),
                Err(e) => {
                    // Only try path-based deletion if the ID was not found
                    if matches!(e, codesearch::DomainError::NotFound(_)) {
                        use_case.delete_by_path(&id_or_path).await?;
                        println!("Repository deleted successfully.");
                    } else {
                        return Err(e.into());
                    }
                }
            }
        }

        Commands::Stats => {
            let list_use_case = ListRepositoriesUseCase::new(sqlite.clone());
            let repos = list_use_case.execute().await?;

            let total_repos = repos.len();
            let total_files: u64 = repos.iter().map(|r| r.file_count()).sum();
            let total_chunks: u64 = repos.iter().map(|r| r.chunk_count()).sum();

            println!("CodeSearch Statistics");
            println!("=====================");
            println!("Repositories: {}", total_repos);
            println!("Total Files:  {}", total_files);
            println!("Total Chunks: {}", total_chunks);
            println!("Data Dir:     {}", data_dir_str);
        }
    }

    Ok(())
}

fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            if path == "~" {
                return home.to_string_lossy().to_string();
            }
            return path.replacen("~", &home.to_string_lossy(), 1);
        }
    }
    path.to_string()
}
