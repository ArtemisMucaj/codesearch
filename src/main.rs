use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
use rmcp::ServiceExt;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
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

/// JSON log file written inside the data directory (alongside `config.json`).
const LOG_FILE: &str = "codesearch.log";

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

    #[arg(long, global = true, default_value = codesearch::cli::DEFAULT_NAMESPACE, value_parser = validate_namespace)]
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

    /// LLM provider for query expansion: 'open-ai' (default), 'anthropic', or 'copilot'
    #[arg(long, global = true, value_enum, default_value = "open-ai")]
    llm_target: LlmTarget,

    /// Force direct database access for write commands even if a `serve`
    /// process is running. Fails with a DuckDB lock error if the server still
    /// holds the write lock — use only when you know the server is stopped.
    #[arg(long, global = true)]
    local: bool,

    /// Route write commands (create/index/delete) through the management API of
    /// a specific running server (e.g. http://127.0.0.1:8676) instead of
    /// auto-detecting one. Implies not opening the database directly.
    #[arg(long, global = true)]
    server: Option<String>,

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
    // `copilot login` opens a full-screen model picker (ratatui), so it needs
    // the same "logs to a file, not the corrupted terminal" treatment as the
    // TUI. `models`/`status` are plain stdout and don't, but treating the whole
    // `copilot` command uniformly keeps the branch simple and harmless.
    let is_copilot = matches!(&cli.command, Commands::Copilot { .. });
    // `openai select` opens a ratatui picker, so like `copilot` it needs
    // file-based logging; the other openai subcommands are plain stdout but the
    // uniform branch is harmless.
    let is_openai = matches!(&cli.command, Commands::Openai { .. });

    // All logs are written as JSON to a file under the data directory (where the
    // config lives, default `~/.codesearch/codesearch.log`) regardless of the
    // command. Ratatui-owning commands and MCP stdio mode need this because any
    // write to the terminal / stderr would corrupt their output channel; for the
    // plain CLI it keeps a durable, machine-readable record.
    //
    // The env filter gates what reaches the file: `warn` for dependencies and
    // `info` for codesearch (`debug` with `--verbose`).
    let filter = if cli.verbose {
        EnvFilter::new("warn,codesearch=debug")
    } else {
        EnvFilter::new("warn,codesearch=info")
    };

    let data_dir = expand_tilde(&cli.data_dir);
    // Creating the data dir and opening the log file are blocking syscalls, so
    // run them off the async runtime in one hop (per the project's async rule).
    let log_file = {
        let data_dir = data_dir.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir)?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::path::Path::new(&data_dir).join(LOG_FILE))
        })
        .await?
        .map_err(|e| anyhow::anyhow!("Failed to open log file: {}", e))?
    };
    let json_file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_target(false)
        .with_writer(log_file)
        .with_filter(filter);

    // A ratatui picker or the MCP stdio protocol owns the terminal, so nothing
    // may be written to the console there. For the plain CLI we additionally
    // surface ERROR-level logs to stderr in a human-readable text format — no
    // warn/info/debug, so routine output stays clean.
    let owns_terminal = is_tui || is_copilot || is_openai;
    let is_mcp_stdio = is_mcp && http_port.is_none();
    let console_error_layer = if owns_terminal || is_mcp_stdio {
        None
    } else {
        Some(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(std::io::stderr)
                .with_filter(LevelFilter::ERROR),
        )
    };

    tracing_subscriber::registry()
        .with(json_file_layer)
        .with(console_error_layer)
        .init();

    if cli.embedding_requests == 0 {
        eprintln!("error: --embedding-requests must be greater than 0");
        std::process::exit(1);
    }

    let db_path = std::path::Path::new(&data_dir).join("codesearch.duckdb");

    // Write commands (create / index / delete) need a DuckDB write lock. If a
    // `serve` process is running against this data-dir it already holds that
    // lock, so route the command through its management API instead of failing.
    // Read commands don't come here — they open the DB read-only, which is
    // allowed alongside the writer. `--local` forces direct access; `--server`
    // forces a specific target. On a server-owned dir where the API call fails,
    // we error out (rather than silently hitting the lock).
    if matches!(
        &cli.command,
        Commands::Create { .. } | Commands::Index { .. } | Commands::Delete { .. }
    ) {
        use codesearch::cli::server_client::{resolve_route, RouteTarget, ServerClient};
        let route = resolve_route(
            std::path::Path::new(&data_dir),
            cli.local,
            cli.server.as_deref(),
        )
        .await?;
        if let RouteTarget::Server(base) = route {
            let client = ServerClient::new(base);
            let output = match &cli.command {
                Commands::Create {
                    name,
                    embedding_target,
                    embedding_model,
                    embedding_dimensions,
                    no_embeddings,
                } => {
                    let namespace = name.as_deref().unwrap_or(&cli.namespace);
                    let target = match embedding_target {
                        EmbeddingTarget::Onnx => "onnx",
                        EmbeddingTarget::Api => "api",
                    };
                    client
                        .create_namespace(
                            namespace,
                            target,
                            embedding_model.as_deref(),
                            *embedding_dimensions,
                            *no_embeddings,
                        )
                        .await?
                }
                Commands::Index { path, name, force } => {
                    // Only forward a namespace the user actually asked for:
                    // `--namespace` has a default, so sending it unconditionally
                    // would make every routed index carry one — which an
                    // in-memory server rejects, breaking a plain `index .`.
                    let namespace =
                        flag_set(&matches, "namespace").then_some(cli.namespace.as_str());
                    client
                        .index_repository(path, name.as_deref(), namespace, *force)
                        .await?
                }
                Commands::Delete { id_or_path } => client.delete_repository(id_or_path).await?,
                _ => unreachable!("guarded by the matches! above"),
            };
            println!("{output}");
            return Ok(());
        }
        // RouteTarget::Local — fall through to the direct-DB paths below.
    }

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

    // `copilot` (login / models / status) only needs the data directory and the
    // `copilot` CLI — no index database, embeddings, or container. Handle it
    // before the container is built so it starts instantly and never loads ONNX.
    if is_copilot {
        let Commands::Copilot { subcommand } = cli.command else {
            unreachable!("is_copilot is only set for Commands::Copilot")
        };
        let output = codesearch::run_copilot_command(subcommand, &data_dir).await?;
        println!("{output}");
        return Ok(());
    }

    // `openai` (endpoints / add / use / models / select) only needs the data
    // directory and the target server — no container. Handle it early too.
    if is_openai {
        let Commands::Openai { subcommand } = cli.command else {
            unreachable!("is_openai is only set for Commands::Openai")
        };
        let output = codesearch::run_openai_command(subcommand, &data_dir).await?;
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
                | Commands::Couplings { .. }
                | Commands::Visualize { .. }
                | Commands::Tui { .. }
        );

    // `data_dir` is moved into ContainerConfig below; keep a copy for the serve
    // block's run-info file (written next to the DB in this directory).
    let data_dir_for_runinfo = data_dir.clone();

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
            // Stdio mode.
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

        // Advertise this server so one-shot CLI invocations against the same
        // data-dir route their write commands through it instead of hitting the
        // DuckDB write lock we hold. Removed after the servers exit; a stale
        // file (hard kill) is caught by the CLI's /health probe.
        let runinfo = codesearch::ServeRunInfo {
            mgmt_port: serve_mgmt_port,
            mcp_port: serve_mcp_port,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        codesearch::write_runinfo(std::path::Path::new(&data_dir_for_runinfo), &runinfo);

        let mcp = run_http_server(container.clone(), serve_mcp_port, serve_public);
        let mgmt = codesearch::run_management_server(container, serve_mgmt_port, serve_public);

        tracing::info!(
            "codesearch serve: MCP on port {}, management API on port {}",
            serve_mcp_port,
            serve_mgmt_port
        );

        // The HTTP servers shut down on ctrl-c (SIGINT), but a supervising app
        // (e.g. Hoplon) stops the process with SIGTERM. On Unix, watch for
        // SIGTERM too so the run-info file is removed on that path rather than
        // leaking. A leaked file isn't fatal — the CLI's /health probe rejects a
        // stale one — but cleaning up keeps the common case tidy.
        #[cfg(unix)]
        let result: Result<()> = {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            tokio::select! {
                res = mcp => res,
                res = mgmt => res,
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM; shutting down serve");
                    Ok(())
                }
            }
        };
        #[cfg(not(unix))]
        let result: Result<()> = tokio::select! {
            res = mcp => res,
            res = mgmt => res,
        };

        codesearch::remove_runinfo(std::path::Path::new(&data_dir_for_runinfo));
        result?;
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
