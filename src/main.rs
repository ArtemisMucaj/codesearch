use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use codesearch::{
    ChromaVectorRepository, DeleteRepositoryUseCase, DuckdbMetadataRepository,
    DuckdbVectorRepository, EmbeddingService, InMemoryVectorRepository, IndexRepositoryUseCase,
    ListRepositoriesUseCase, MockEmbedding, MockReranking, OrtEmbedding, OrtReranking,
    RerankingService, SearchCodeUseCase, SearchQuery, TreeSitterParser, VectorRepository,
    VectorStore,
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
    chroma_url: Option<String>,

    #[arg(long, global = true, default_value = "search")]
    namespace: String,

    #[arg(long, global = true)]
    memory_storage: bool,

    #[arg(long, global = true)]
    no_rerank: bool,

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

        #[arg(long, default_value = "10")]
        num: usize,

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

    let level = if cli.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };
    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let data_dir = expand_tilde(&cli.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let db_path = PathBuf::from(&data_dir).join("codesearch.duckdb");

    let parser = Arc::new(TreeSitterParser::new());
    let embedding_service: Arc<dyn EmbeddingService> = if cli.mock_embeddings {
        info!("Using mock embedding service");
        Arc::new(MockEmbedding::new())
    } else {
        info!("Initializing ONNX embedding service...");
        Arc::new(OrtEmbedding::new(None)?)
    };

    let reranking_service: Option<Arc<dyn RerankingService>> = if !cli.no_rerank {
        if cli.mock_embeddings {
            info!("Using mock reranking service");
            Some(Arc::new(MockReranking::new()))
        } else {
            info!("Initializing ONNX reranking service...");
            match OrtReranking::new(None) {
                Ok(reranker) => Some(Arc::new(reranker)),
                Err(e) => {
                    tracing::warn!(
                        "Failed to initialize reranking service: {}. Continuing without reranking.",
                        e
                    );
                    None
                }
            }
        }
    } else {
        None
    };

    // Create vector repository first to ensure it gets write access to DuckDB
    // (DuckDB only allows one write connection per file)
    // We also need to share the connection with the repository metadata adapter.
    let (vector_repo, repo_adapter): (Arc<dyn VectorRepository>, Arc<DuckdbMetadataRepository>) =
        if cli.memory_storage {
            info!("Using in-memory vector storage");
            let vector = Arc::new(InMemoryVectorRepository::new());
            let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
            (vector, repo_adapter)
        } else if let Some(chroma_url) = cli.chroma_url.as_deref() {
            match ChromaVectorRepository::new(chroma_url, &cli.namespace).await {
                Ok(chroma) => {
                    info!(
                        "Connected to ChromaDB at {} namespace {}",
                        chroma_url, cli.namespace
                    );
                    let vector = Arc::new(chroma);
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    (vector, repo_adapter)
                }
                Err(e) => {
                    tracing::warn!(
                    "Failed to connect to ChromaDB ({}): {}. Falling back to in-memory storage.",
                    chroma_url,
                    e
                );
                    let vector = Arc::new(InMemoryVectorRepository::new());
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    (vector, repo_adapter)
                }
            }
        } else {
            // DuckDB vector storage - share connection with repository adapter
            match DuckdbVectorRepository::new_with_namespace(&db_path, &cli.namespace) {
                Ok(duckdb) => {
                    info!(
                        "Using DuckDB vector storage at {:?} namespace {}",
                        db_path, cli.namespace
                    );
                    // Share the connection with the repository adapter
                    let shared_conn = duckdb.shared_connection();
                    let repo_adapter =
                        Arc::new(DuckdbMetadataRepository::with_connection(shared_conn)?);
                    (Arc::new(duckdb), repo_adapter)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to initialize DuckDB ({}): {}. Falling back to in-memory storage.",
                        db_path.display(),
                        e
                    );
                    let vector = Arc::new(InMemoryVectorRepository::new());
                    let repo_adapter = Arc::new(DuckdbMetadataRepository::new(&db_path)?);
                    (vector, repo_adapter)
                }
            }
        };

    match cli.command {
        Commands::Index { path, name } => {
            let (vector_store, ns): (VectorStore, Option<String>) = if cli.memory_storage {
                (VectorStore::InMemory, None)
            } else if cli.chroma_url.is_some() {
                (VectorStore::ChromaDb, Some(cli.namespace.clone()))
            } else {
                (VectorStore::DuckDb, Some(cli.namespace.clone()))
            };

            let use_case = IndexRepositoryUseCase::new(
                repo_adapter.clone(),
                vector_repo.clone(),
                parser,
                embedding_service,
            );

            let repo = use_case
                .execute(&path, name.as_deref(), vector_store, ns)
                .await?;
            println!(
                "Successfully indexed repository: {} ({} files, {} chunks)",
                repo.name(),
                repo.file_count(),
                repo.chunk_count()
            );
        }

        Commands::Search {
            query,
            num,
            min_score,
            language,
            repository,
        } => {
            let mut use_case = SearchCodeUseCase::new(vector_repo.clone(), embedding_service);

            if let Some(reranker) = reranking_service {
                use_case = use_case.with_reranking(reranker);
            }

            let mut search_query = SearchQuery::new(&query).with_limit(num);

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
                        .take(10)
                        .map(|l| format!("   | {}", l))
                        .collect::<Vec<_>>()
                        .join("\n");
                    println!("{}", preview);
                    println!();
                }
            }
        }

        Commands::List => {
            let use_case = ListRepositoriesUseCase::new(repo_adapter.clone());
            let repos = use_case.execute().await?;

            if repos.is_empty() {
                println!("No repositories indexed.");
            } else {
                println!("Indexed repositories:\n");
                for repo in repos {
                    println!("  {} ({})", repo.name(), repo.id());
                    println!("    Path: {}", repo.path());
                    println!(
                        "    Files: {}, Chunks: {}",
                        repo.file_count(),
                        repo.chunk_count()
                    );
                    let ns_display = repo.namespace().unwrap_or("(none)");
                    println!(
                        "    Store: {}, Namespace: {}",
                        repo.store().as_str(),
                        ns_display
                    );
                    println!();
                }
            }
        }

        Commands::Delete { id_or_path } => {
            let use_case = DeleteRepositoryUseCase::new(repo_adapter.clone(), vector_repo.clone());

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
            let list_use_case = ListRepositoriesUseCase::new(repo_adapter.clone());
            let repos = list_use_case.execute().await?;

            let total_repos = repos.len();
            let total_files: u64 = repos.iter().map(|r| r.file_count()).sum();
            let total_chunks: u64 = repos.iter().map(|r| r.chunk_count()).sum();

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

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn use_chroma_flag_is_removed() {
        let res = Cli::try_parse_from(["codesearch", "--use-chroma", "stats"]);
        assert!(res.is_err(), "--use-chroma should not be a valid flag");
    }
}
