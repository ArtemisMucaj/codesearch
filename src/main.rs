use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use codesearch::cli::{validate_namespace, EmbeddingTarget, LlmTarget, RerankingTarget};
use codesearch::connector::adapter::mcp::CodesearchMcpServer;
use codesearch::{
    Commands, Container, ContainerConfig, DuckdbVectorRepository, NamespaceEmbeddingConfig, Router,
    DEFAULT_ONNX_EMBEDDING_MODEL, NO_EMBEDDINGS_MODEL,
};

/// Default embedding dimensionality for namespaces created (or first indexed)
/// without an explicit `--embedding-dimensions` (matches all-MiniLM-L6-v2).
const DEFAULT_EMBEDDING_DIMENSIONS: usize = 384;

/// Handle `codesearch create`: persist the namespace's embedding
/// configuration without loading any embedding model.
fn create_namespace(
    db_path: &std::path::Path,
    namespace: &str,
    target: EmbeddingTarget,
    model: Option<&str>,
    dimensions: usize,
    no_embeddings: bool,
) -> Result<String> {
    if dimensions == 0 {
        anyhow::bail!("--embedding-dimensions must be greater than 0");
    }

    let (embedding_target, embedding_model) = if no_embeddings {
        (
            NO_EMBEDDINGS_MODEL.to_string(),
            NO_EMBEDDINGS_MODEL.to_string(),
        )
    } else {
        match target {
            EmbeddingTarget::Onnx => (
                "onnx".to_string(),
                model.unwrap_or(DEFAULT_ONNX_EMBEDDING_MODEL).to_string(),
            ),
            EmbeddingTarget::Api => {
                let model = model.ok_or_else(|| {
                    anyhow::anyhow!("--embedding-model is required with --embedding-target=api")
                })?;
                ("api".to_string(), model.to_string())
            }
        }
    };

    let description = if no_embeddings {
        "no embeddings — keyword + call-graph search only".to_string()
    } else {
        format!(
            "target '{}', model '{}', {} dimensions",
            embedding_target, embedding_model, dimensions
        )
    };

    let cfg = NamespaceEmbeddingConfig {
        embedding_target,
        embedding_model,
        dimensions,
    };
    DuckdbVectorRepository::create_namespace(db_path, namespace, &cfg)?;

    Ok(format!(
        "Created namespace '{}' ({}).\nIndex into it with: codesearch index <path> --namespace {}",
        namespace, description, namespace
    ))
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
    // `serve` runs BOTH the MCP HTTP server and the management API together.
    let (is_serve, serve_mcp_port, serve_mgmt_port, serve_public) = match &cli.command {
        Commands::Serve {
            mcp_port,
            mgmt_port,
            public,
        } => (true, *mcp_port, *mgmt_port, *public),
        _ => (false, 0, 0, false),
    };
    let is_tui = matches!(&cli.command, Commands::Tui { .. });
    // `memory import` with no PATH opens an interactive picker (a full-screen
    // TUI) before any container is built. It owns the terminal like the TUI, so
    // it needs the same file-based logging and the same "open first, build the
    // heavy container only afterwards" treatment.
    let is_import_picker = matches!(
        &cli.command,
        Commands::Memory {
            subcommand: codesearch::MemorySubcommand::Import { path: None, .. },
        }
    );

    // For MCP stdio mode, log to stderr (stdout is for MCP protocol)
    // For HTTP mode, we can log to stdout since HTTP uses a different channel
    // For TUI mode, log to a file so ratatui's terminal is not corrupted
    let filter = if cli.verbose {
        EnvFilter::new("warn,codesearch=debug")
    } else {
        EnvFilter::new("warn,codesearch=info")
    };

    if is_tui || is_import_picker {
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

    if cli.embedding_requests == 0 {
        eprintln!("error: --embedding-requests must be greater than 0");
        std::process::exit(1);
    }

    let data_dir = expand_tilde(&cli.data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let db_path = std::path::Path::new(&data_dir).join("codesearch.duckdb");

    // `create` only writes namespace configuration — handle it before the
    // container is built so no embedding model is loaded or downloaded.
    if let Commands::Create {
        name,
        embedding_target,
        embedding_model,
        embedding_dimensions,
        no_embeddings,
    } = &cli.command
    {
        let namespace = name.as_deref().unwrap_or(&cli.namespace);
        let output = create_namespace(
            &db_path,
            namespace,
            *embedding_target,
            embedding_model.as_deref(),
            *embedding_dimensions,
            *no_embeddings,
        )?;
        println!("{output}");
        return Ok(());
    }

    // Auto-resolve the namespace from the indexed metadata so commands run
    // from inside a repository "just work", then adopt that namespace's
    // stored embedding configuration — written by `codesearch create` or by
    // the first index run — as the source of truth. Embedding settings are
    // never taken from the command line outside `codesearch create`.
    let mut namespace = cli.namespace.clone();
    let mut embedding_target = EmbeddingTarget::Onnx;
    let mut embedding_model: Option<String> = None;
    let mut embedding_dimensions = DEFAULT_EMBEDDING_DIMENSIONS;
    let mut no_embeddings = false;

    if !cli.memory_storage {
        if !flag_set(&matches, "namespace") {
            let repo_root = match &cli.command {
                Commands::Index { path, .. } => std::fs::canonicalize(path).ok(),
                _ => std::env::current_dir().ok(),
            };
            if let Some(ctx) =
                repo_root.and_then(|root| codesearch::resolve_repo_context(&db_path, &root))
            {
                namespace = ctx.namespace.clone();
                tracing::info!(
                    "Using namespace '{}' (matched by {} for '{}') from indexed metadata",
                    namespace,
                    ctx.matched_by,
                    ctx.repository_name
                );
            }
        }

        if let Some(ns_cfg) = codesearch::namespace_embedding_config(&db_path, &namespace) {
            no_embeddings = ns_cfg.embedding_model == codesearch::NO_EMBEDDINGS_MODEL
                || ns_cfg.embedding_target == codesearch::NO_EMBEDDINGS_MODEL;
            embedding_dimensions = ns_cfg.dimensions;
            if !no_embeddings {
                embedding_target = match ns_cfg.embedding_target.as_str() {
                    "api" => EmbeddingTarget::Api,
                    _ => EmbeddingTarget::Onnx,
                };
                embedding_model = Some(ns_cfg.embedding_model);
            }
            tracing::info!(
                "Using embedding configuration stored for namespace '{}'",
                namespace
            );
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
                | Commands::Channels { .. }
                | Commands::Uses { .. }
                | Commands::Visualize { .. }
                | Commands::Tui { .. }
                // Memory commands only touch memory.duckdb, never the code
                // index, so the index database can stay read-only.
                | Commands::Memory { .. }
        );

    let config = ContainerConfig {
        data_dir,
        mock_embeddings: cli.mock_embeddings,
        namespace,
        memory_storage: cli.memory_storage,
        no_rerank: cli.no_rerank,
        no_embeddings,
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

    // `serve` runs the MCP HTTP server and the REST/JSON management API
    // concurrently. Neither blocks the other: both are driven under a single
    // `tokio::select!` so ctrl-c (each server shuts down gracefully on it) or
    // an error in either one tears the whole command down.
    if is_serve {
        if serve_mcp_port == serve_mgmt_port {
            anyhow::bail!("--mcp-port and --mgmt-port must differ (both were {serve_mcp_port})");
        }

        let container = Arc::new(Container::new(config).await?);

        let mcp = run_http_server(container.clone(), serve_mcp_port, serve_public);
        let mgmt = codesearch::run_management_server(container, serve_mgmt_port, serve_public);

        tracing::info!(
            "codesearch serve: MCP on port {}, management API on port {}",
            serve_mcp_port,
            serve_mgmt_port
        );

        tokio::select! {
            res = mcp => res?,
            res = mgmt => res?,
        }
        return Ok(());
    }

    // Interactive `memory import`: open the picker BEFORE building the
    // container so the TUI appears instantly instead of waiting for ONNX models
    // to load. Discovery streams into the picker on background threads. Only if
    // the user selects sessions do we build the (heavy) container and extract.
    if is_import_picker {
        if let Commands::Memory {
            subcommand: codesearch::MemorySubcommand::Import { llm, .. },
        } = cli.command
        {
            use codesearch::tui::import_picker::{ImportEvent, ImportRequest};

            // Two channels bridge the (blocking) picker UI and the (async)
            // import worker: requests flow UI → worker (a tokio channel so the
            // worker `recv().await`s instead of pinning a runtime thread),
            // progress flows back over a std channel the picker drains by poll.
            let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel::<ImportRequest>();
            let (evt_tx, evt_rx) = std::sync::mpsc::channel::<ImportEvent>();

            // Worker: build the container (loads models) in the background, then
            // serve import requests until the picker closes the request channel.
            // The picker is already interactive while this runs.
            let worker = tokio::spawn(async move {
                let container = match Container::new(config).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = evt_tx.send(ImportEvent::ContainerFailed {
                            error: e.to_string(),
                        });
                        return;
                    }
                };
                let controller = codesearch::MemoryController::new(&container);
                if let Err(e) = controller.serve_import_requests(req_rx, evt_tx, llm).await {
                    tracing::error!("import worker failed: {e}");
                }
            });

            // The picker owns the terminal; run it on a blocking thread so it
            // never contends with the async runtime's reactor. Dropping req_tx
            // when it returns signals the worker to finish.
            let ui = tokio::task::spawn_blocking(move || {
                codesearch::run_import_picker_ui(evt_rx, req_tx)
            })
            .await
            .map_err(|e| anyhow::anyhow!("session picker task panicked: {e}"))?;
            ui?;

            // The picker closed; the request channel is dropped, so the worker
            // loop ends. Wait for any in-flight import to finish cleanly.
            let _ = worker.await;
            return Ok(());
        }
        unreachable!("is_import_picker is only set for Memory::Import with no path")
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
