use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use codesearch::cli::{EmbeddingTarget, LlmTarget, RerankingTarget};
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

    /// Embedding backend: 'onnx' for bundled ONNX models (default, offline-capable)
    /// or 'api' for an OpenAI-compatible /v1/embeddings endpoint (e.g. LM Studio).
    /// The chosen target and model are stored per namespace on first index and
    /// validated on every subsequent operation — mismatches are hard errors.
    /// Set OPENAI_BASE_URL to override the default http://localhost:1234.
    #[arg(long, global = true, value_enum, default_value = "onnx")]
    embedding_target: EmbeddingTarget,

    /// Embedding model identifier.
    /// For 'onnx': HuggingFace model ID (default: sentence-transformers/all-MiniLM-L6-v2).
    /// For 'api': model name sent in the /v1/embeddings request body; must match
    /// the model loaded in the target server (set OPENAI_BASE_URL for non-default address).
    #[arg(long, global = true)]
    embedding_model: Option<String>,

    /// Number of dimensions produced by the embedding model (default: 384).
    /// Override when using a model with a different output size (e.g. 768 or 1024).
    /// This value is stored in namespace_config on first index and cannot be
    /// changed without re-indexing.
    #[arg(long, global = true, default_value = "384")]
    embedding_dimensions: usize,

    /// Reranking backend (used when --no-rerank is not set):
    ///   'onnx'           — bundled ONNX cross-encoder model (default).
    ///   'api/anthropic'  — LLM via /v1/messages (ANTHROPIC_BASE_URL, ANTHROPIC_MODEL).
    ///   'api/openai'     — LLM via /v1/chat/completions (OPENAI_BASE_URL, OPENAI_MODEL).
    #[arg(long, global = true, value_enum, default_value = "onnx")]
    reranking_target: RerankingTarget,

    /// Maximum number of concurrent embedding API calls during indexing.
    ///
    /// For the API embedding target ('--embedding-target=api'), each slot is a
    /// parallel HTTP request to the embedding server, so higher values reduce
    /// indexing time proportionally. For the ONNX target, each slot is a
    /// spawn_blocking task — gains are bounded by available CPU cores.
    ///
    /// Default: 4.
    #[arg(long, global = true, default_value = "4")]
    embedding_requests: usize,

    /// Provider for LLM-based query expansion (used with --expand-query):
    ///   'anthropic' — /v1/messages (ANTHROPIC_BASE_URL, ANTHROPIC_MODEL, default).
    ///   'open-ai'   — /v1/chat/completions (OPENAI_BASE_URL, OPENAI_MODEL).
    #[arg(long, global = true, value_enum, default_value = "anthropic")]
    llm_target: LlmTarget,

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
    let is_tui = matches!(&cli.command, Commands::Tui { .. });

    // For MCP stdio mode, log to stderr (stdout is for MCP protocol)
    // For HTTP mode, we can log to stdout since HTTP uses a different channel
    // For TUI mode, log to a file so ratatui's terminal is not corrupted
    let filter = if cli.verbose {
        EnvFilter::new("warn,codesearch=debug")
    } else {
        EnvFilter::new("warn,codesearch=info")
    };

    if is_tui {
        // Ratatui owns the terminal; any write to stderr corrupts the display.
        // Redirect logs to ~/.codesearch/tui.log so they are still accessible.
        let log_dir = expand_tilde(&cli.data_dir);
        std::fs::create_dir_all(&log_dir)?;
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/tui.log", log_dir))
            .map_err(|e| anyhow::anyhow!("Failed to open TUI log file: {}", e))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(log_file)
            .with_ansi(false)
            .init();
    } else if is_mcp && http_port.is_none() {
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

    if cli.embedding_dimensions == 0 {
        eprintln!("error: --embedding-dimensions must be greater than 0");
        std::process::exit(1);
    }
    if cli.embedding_requests == 0 {
        eprintln!("error: --embedding-requests must be greater than 0");
        std::process::exit(1);
    }

    let data_dir = expand_tilde(&cli.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    // Read-only mode for commands that never write to the database.
    // This avoids acquiring DuckDB's exclusive write lock, allowing multiple
    // codesearch processes (e.g. concurrent searches) to run simultaneously.
    let read_only = !is_mcp
        && matches!(
            &cli.command,
            Commands::Search { .. }
                | Commands::List
                | Commands::Stats
                | Commands::Impact { .. }
                | Commands::Context { .. }
                | Commands::Explain { .. }
                | Commands::Graph { .. }
                | Commands::Split { .. }
                | Commands::Tui { .. }
        );

    let config = ContainerConfig {
        data_dir,
        mock_embeddings: cli.mock_embeddings,
        namespace: cli.namespace,
        memory_storage: cli.memory_storage,
        no_rerank: cli.no_rerank,
        expand_query: cli.expand_query,
        embedding_target: cli.embedding_target,
        embedding_model: cli.embedding_model,
        embedding_dimensions: cli.embedding_dimensions,
        reranking_target: cli.reranking_target,
        llm_target: cli.llm_target,
        parse_concurrency: cli.embedding_requests,
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

    let container = if is_tui {
        // For TUI: take over the terminal immediately so the user sees the UI
        // at once, then load the ONNX models in the background.  The TUI event
        // loop wakes up when `ContainerReady` arrives on the mpsc channel.
        if let Commands::Tui {
            repository,
            query,
            mode,
        } = cli.command
        {
            use codesearch::tui::event::TuiEvent;
            use codesearch::tui::TuiApp;
            use tokio::sync::mpsc;

            let mut terminal = ratatui::init();

            let (tx, rx) = mpsc::unbounded_channel::<TuiEvent>();
            let tx_bg = tx.clone();

            // Spawn container init as a background task so the TUI is
            // immediately interactive while models are compiling.
            // Capture the handle so we can detect panics and forward them to
            // the UI rather than leaving it in a perpetual loading state.
            let handle = tokio::spawn(async move {
                Container::new(config)
                    .await
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            });
            tokio::spawn(async move {
                let result = match handle.await {
                    Ok(r) => r,
                    Err(join_err) => Err(format!("container init panicked: {join_err}")),
                };
                // Ignore send errors: the user may have quit before models loaded.
                let _ = tx_bg.send(TuiEvent::ContainerReady(result));
            });

            let mut app = TuiApp::new_loading(repository, mode, query, tx, rx);
            let result = app.run_with_terminal(&mut terminal).await;
            ratatui::restore();
            return result;
        }

        // Unreachable: is_tui is only true when cli.command is Commands::Tui,
        // and the branch above always returns.
        unreachable!("TUI command variant not matched")
    } else {
        Container::new(config).await?
    };

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

    let app = Router::new().route(
        "/mcp",
        any(move |req| async move { mcp_service.handle(req).await }),
    );

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
