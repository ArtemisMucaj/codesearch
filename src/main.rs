use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use codesearch::{Commands, Container, ContainerConfig, Router};

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Configure logging: only show codesearch logs, suppress noisy external crates
    let filter = if cli.verbose {
        EnvFilter::new("warn,codesearch=debug")
    } else {
        EnvFilter::new("warn,codesearch=info")
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let data_dir = expand_tilde(&cli.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let config = ContainerConfig {
        data_dir,
        mock_embeddings: cli.mock_embeddings,
        chroma_url: cli.chroma_url,
        namespace: cli.namespace,
        memory_storage: cli.memory_storage,
        no_rerank: cli.no_rerank,
    };

    let container = Container::new(config).await?;
    let router = Router::new(&container);
    let output = router.route(cli.command).await?;

    println!("{}", output);

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
