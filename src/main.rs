use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use codesearch::connector::adapter::mcp::CodesearchMcpServer;
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

    #[arg(long, global = true, default_value = "search")]
    namespace: String,

    #[arg(long, global = true)]
    memory_storage: bool,

    #[arg(long, global = true)]
    no_rerank: bool,

    /// Enable query expansion: the search query is automatically expanded into
    /// multiple variants before searching. Results are fused via RRF for better
    /// recall. The LLM service is determined by ANTHROPIC_BASE_URL (default:
    /// http://localhost:1234, targeting a local LM Studio instance).
    /// ANTHROPIC_API_KEY is not required when targeting a local endpoint.
    /// If expansion fails for any reason the original query is used as-is.
    #[arg(long, global = true)]
    expand_query: bool,

    #[command(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Extract MCP mode info before moving cli.command
    let (is_mcp, http_port, public_bind) = match &cli.command {
        Commands::Mcp { http, public } => (true, *http, *public),
        _ => (false, None, false),
    };

    // For MCP stdio mode, log to stderr (stdout is for MCP protocol)
    // For HTTP mode, we can log to stdout since HTTP uses a different channel
    let filter = if cli.verbose {
        EnvFilter::new("warn,codesearch=debug")
    } else {
        EnvFilter::new("warn,codesearch=info")
    };

    if is_mcp && http_port.is_none() {
        // Stdio mode - log to stderr
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    }

    let data_dir = expand_tilde(&cli.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    // Read-only mode for commands that never write to the database.
    // This avoids acquiring DuckDB's exclusive write lock, allowing multiple
    // codesearch processes (e.g. concurrent searches) to run simultaneously.
    let read_only = !is_mcp
        && matches!(
            &cli.command,
            Commands::Search { .. } | Commands::List | Commands::Stats
        );

    let config = ContainerConfig {
        data_dir,
        mock_embeddings: cli.mock_embeddings,
        namespace: cli.namespace,
        memory_storage: cli.memory_storage,
        no_rerank: cli.no_rerank,
        expand_query: cli.expand_query,
        read_only,
    };

    // Handle MCP command specially - it runs as a long-lived server
    if is_mcp {
        let container = Arc::new(Container::new(config).await?);

        if let Some(port) = http_port {
            // HTTP mode
            run_http_server(container, port, public_bind).await?;
        } else {
            // Stdio mode
            tracing::info!("Starting codesearch MCP server (stdio)");
            let server = CodesearchMcpServer::new(container);
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        return Ok(());
    }

    let container = Container::new(config).await?;
    let router = Router::new(&container);
    let output = router.route(cli.command).await?;

    println!("{}", output);

    Ok(())
}

async fn run_http_server(container: Arc<Container>, port: u16, public: bool) -> Result<()> {
    use axum::routing::any;
    use axum::Router;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;
    use tokio_util::sync::CancellationToken;

    let bind_addr: [u8; 4] = if public { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
    let addr = SocketAddr::from((bind_addr, port));

    tracing::info!("Starting codesearch MCP server (HTTP) on {}", addr);

    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig {
        sse_keep_alive: Some(std::time::Duration::from_secs(15)),
        sse_retry: None,
        stateful_mode: true,
        cancellation_token: ct.clone(),
    };

    let session_manager = Arc::new(LocalSessionManager::default());

    let mcp_service = StreamableHttpService::new(
        move || Ok(CodesearchMcpServer::new(container.clone())),
        session_manager,
        config,
    );

    let app = Router::new().route("/mcp", any(move |req| async move { mcp_service.handle(req).await }));

    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("MCP HTTP server listening on http://{}/mcp", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down MCP HTTP server");
            ct.cancel();
        })
        .await?;

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
