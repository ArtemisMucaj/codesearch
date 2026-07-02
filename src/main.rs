use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use codesearch::cli::{EmbeddingTarget, LlmTarget, RerankingTarget};
use codesearch::connector::adapter::mcp::CodesearchMcpServer;
use codesearch::{Commands, Container, ContainerConfig, Router};

/// Validates a namespace for use as a DuckDB schema name.
///
/// Schema names are always double-quoted in generated SQL, so almost any
/// character is safe. The one character that cannot appear is `"` itself,
/// because it would break the quoting even after standard `""` escaping in
/// the FTS PRAGMA argument (which is a SQL string, not a full SQL statement).
fn validate_namespace(s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("namespace must not be empty".to_string());
    }
    if s.contains('"') {
        return Err(format!(
            "namespace '{s}' contains '\"', which is not allowed in a namespace."
        ));
    }
    Ok(s.to_string())
}

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

    #[arg(long, global = true, default_value = "search", value_parser = validate_namespace)]
    namespace: String,

    #[arg(long, global = true)]
    memory_storage: bool,

    #[arg(long, global = true)]
    no_rerank: bool,

    /// Expand the query into variants before searching and fuse results via RRF
    #[arg(long, global = true)]
    expand_query: bool,

    /// Embedding backend: 'onnx' (bundled, offline) or 'api' (OpenAI-compatible endpoint)
    #[arg(long, global = true, value_enum, default_value = "onnx")]
    embedding_target: EmbeddingTarget,

    /// Embedding model — HuggingFace ID for 'onnx', model name for 'api'
    #[arg(long, global = true)]
    embedding_model: Option<String>,

    /// Output dimensions of the embedding model
    #[arg(long, global = true, default_value = "384")]
    embedding_dimensions: usize,

    /// Reranking backend: 'onnx' (default), 'api/anthropic', or 'api/openai'
    #[arg(long, global = true, value_enum, default_value = "onnx")]
    reranking_target: RerankingTarget,

    /// Max concurrent embedding API calls during indexing
    #[arg(long, global = true, default_value = "4")]
    embedding_requests: usize,

    /// LLM provider for query expansion: 'anthropic' (default) or 'open-ai'
    #[arg(long, global = true, value_enum, default_value = "anthropic")]
    llm_target: LlmTarget,

    #[command(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse via ArgMatches so we can tell which global flags the user actually
    // supplied (vs. their default values) and only auto-resolve the rest.
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

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

    // Auto-resolve the namespace and embedding configuration from the indexed
    // metadata so commands run from inside a repository "just work" without the
    // user re-specifying the flags used at index time. We always resolve (unless
    // in-memory) and adopt the namespace only when it was not pinned explicitly;
    // the embedding settings are adopted only when the effective namespace
    // matches the resolved one, so an explicit `--namespace <indexed-ns>` still
    // picks up that namespace's embedding config (which `Container::new`
    // validates) instead of silently falling back to the ONNX/384 defaults.
    let mut namespace = cli.namespace.clone();
    let mut embedding_target = cli.embedding_target;
    let mut embedding_model = cli.embedding_model.clone();
    let mut embedding_dimensions = cli.embedding_dimensions;

    if !cli.memory_storage {
        let repo_root = match &cli.command {
            Commands::Index { path, .. } => std::fs::canonicalize(path).ok(),
            _ => std::env::current_dir().ok(),
        };
        if let Some(root) = repo_root {
            let db_path = std::path::Path::new(&data_dir).join("codesearch.duckdb");
            if let Some(ctx) = codesearch::resolve_repo_context(&db_path, &root) {
                if !flag_set(&matches, "namespace") {
                    namespace = ctx.namespace.clone();
                }

                // Only adopt the resolved embedding config when it actually
                // describes the namespace we are about to open.
                if namespace == ctx.namespace {
                    if !flag_set(&matches, "embedding_target") {
                        if let Some(target) = ctx.embedding_target.as_deref() {
                            embedding_target = match target {
                                "api" => EmbeddingTarget::Api,
                                _ => EmbeddingTarget::Onnx,
                            };
                        }
                    }
                    if !flag_set(&matches, "embedding_model") && ctx.embedding_model.is_some() {
                        embedding_model = ctx.embedding_model.clone();
                    }
                    if !flag_set(&matches, "embedding_dimensions") {
                        if let Some(dims) = ctx.embedding_dimensions {
                            embedding_dimensions = dims;
                        }
                    }

                    tracing::info!(
                        "Using namespace '{}' (matched by {} for '{}') from indexed metadata",
                        namespace,
                        ctx.matched_by,
                        ctx.repository_name
                    );
                }
            }
        }
    }

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
                | Commands::Features { .. }
                | Commands::Uses { .. }
                | Commands::Visualize { .. }
                | Commands::Tui { .. }
        );

    let config = ContainerConfig {
        data_dir,
        mock_embeddings: cli.mock_embeddings,
        namespace,
        memory_storage: cli.memory_storage,
        no_rerank: cli.no_rerank,
        expand_query: cli.expand_query,
        embedding_target,
        embedding_model,
        embedding_dimensions,
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

/// Whether a (possibly global) argument was supplied on the command line, as
/// opposed to falling back to its default value. Walks into the matched
/// subcommand because global args may be recorded at either level.
fn flag_set(matches: &clap::ArgMatches, id: &str) -> bool {
    use clap::parser::ValueSource;
    fn walk(m: &clap::ArgMatches, id: &str) -> bool {
        if matches!(m.value_source(id), Some(ValueSource::CommandLine)) {
            return true;
        }
        match m.subcommand() {
            Some((_, sub)) => walk(sub, id),
            None => false,
        }
    }
    walk(matches, id)
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
