//! Streaming management endpoints (Server-Sent Events).
//!
//! These endpoints expose long-running / token-streaming codesearch operations
//! to a native app over [Server-Sent Events][sse]. They live under the
//! `/api/stream/...` prefix so they never collide with the non-streaming REST
//! endpoints added by the sibling PR (which own `/api/...`).
//!
//! # SSE event schema
//!
//! Every stream emits newline-delimited SSE frames. Each frame carries a named
//! `event:` and a JSON `data:` payload:
//!
//! | `event`    | Emitted by            | JSON `data` payload                                      |
//! |------------|-----------------------|---------------------------------------------------------|
//! | `token`    | explain               | `{ "text": "<chunk>" }`                                  |
//! | `progress` | index                 | `{ "stage": "<stage>", "message": "<human text>" }`     |
//! | `done`     | explain, index        | operation-specific summary object (see handlers)        |
//! | `error`    | explain, index        | `{ "message": "<human-readable error>" }`               |
//!
//! A terminal `done` **or** `error` event is always the last frame of a stream,
//! after which the server closes the connection. Clients should treat either as
//! end-of-stream. If the client disconnects early, the underlying work is
//! dropped (the spawned task's channel receiver is dropped, cancelling it).
//!
//! [sse]: https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::application::ChatClient;
use crate::cli::LlmTarget;
use crate::connector::adapter::{
    AnthropicClient, CodesearchConfig, CopilotChatClient, OpenAiChatClient,
};
use crate::domain::VectorStore;

use super::server::AppState;

/// Optional JSON body for the explain stream: overrides / extra parameters that
/// don't fit in the path. All fields are optional so a bare `POST` with no body
/// (or a `GET`) still works.
#[derive(Debug, Default, Deserialize)]
pub struct ExplainStreamRequest {
    /// Restrict the explanation to a single repository (name or UUID).
    #[serde(default)]
    pub repository: Option<String>,
    /// Interpret the `:symbol` path segment as a regular expression.
    #[serde(default)]
    pub regex: bool,
    /// Which LLM backend to use. Defaults to the OpenAI-compatible backend.
    #[serde(default)]
    pub llm: Option<StreamLlmTarget>,
    /// Override the model for this request (Copilot backend, and the OpenAI
    /// backend's chosen endpoint). When omitted, the backend's configured/default
    /// model is used. Pick a valid id from `GET /api/llm/models`.
    #[serde(default)]
    pub model: Option<String>,
    /// For the OpenAI backend: which named endpoint from config to use. When
    /// omitted, the configured `active` endpoint (then `OPENAI_*`) is used.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Bypass the cache and recompute: ignore any stored explanation and
    /// overwrite it with a fresh LLM run. What the app's Regenerate button sends.
    #[serde(default)]
    pub regenerate: bool,
}

/// JSON body for the index stream.
#[derive(Debug, Deserialize)]
pub struct IndexStreamRequest {
    /// Filesystem path of the repository to index.
    pub path: String,
    /// Optional human-readable name (defaults to the directory name).
    #[serde(default)]
    pub name: Option<String>,
    /// Force a full re-index, discarding any existing data for this path.
    #[serde(default)]
    pub force: bool,
}

/// Serializable mirror of [`LlmTarget`] so the request body stays decoupled from
/// the CLI enum's derives.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamLlmTarget {
    Anthropic,
    OpenAi,
    Copilot,
}

impl From<StreamLlmTarget> for LlmTarget {
    fn from(value: StreamLlmTarget) -> Self {
        match value {
            StreamLlmTarget::Anthropic => LlmTarget::Anthropic,
            StreamLlmTarget::OpenAi => LlmTarget::OpenAi,
            StreamLlmTarget::Copilot => LlmTarget::Copilot,
        }
    }
}

impl From<LlmTarget> for StreamLlmTarget {
    fn from(value: LlmTarget) -> Self {
        match value {
            LlmTarget::Anthropic => StreamLlmTarget::Anthropic,
            LlmTarget::OpenAi => StreamLlmTarget::OpenAi,
            LlmTarget::Copilot => StreamLlmTarget::Copilot,
        }
    }
}

// ---------------------------------------------------------------------------
// SSE helpers
// ---------------------------------------------------------------------------

/// Build a named SSE event with a JSON `data` payload.
///
/// The event is infallible: on the (practically impossible) chance that the
/// payload fails to serialize, we fall back to a plain error frame so the
/// stream never breaks its own contract.
fn sse_event(name: &str, payload: serde_json::Value) -> Event {
    match Event::default().event(name).json_data(&payload) {
        Ok(event) => event,
        Err(err) => Event::default().event("error").data(
            json!({ "message": format!("failed to serialize SSE payload: {err}") }).to_string(),
        ),
    }
}

/// Turn a receiver of pre-built [`Event`]s into the `Stream` axum's [`Sse`]
/// wants, without pulling in `tokio-stream`. Uses `futures_util::stream::unfold`
/// over the receiver, yielding `Ok(event)` (SSE is infallible here — errors are
/// modelled as `error` events, not stream errors).
fn receiver_stream(
    rx: mpsc::UnboundedReceiver<Event>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    })
}

// ---------------------------------------------------------------------------
// GET/POST /api/stream/explain/:symbol
// ---------------------------------------------------------------------------

/// `GET/POST /api/stream/explain/:symbol` — stream an LLM call-flow explanation.
///
/// Bridges [`ExplainUseCase::execute_streaming`]'s per-token channel onto SSE
/// `token` events, then finishes with a single `done` event carrying the
/// analysis summary (or an `error` event if the LLM/analysis failed).
///
/// The heavy work runs in a spawned task; if the client disconnects, the SSE
/// stream is dropped, which drops the event receiver and lets the spawned task
/// wind down as soon as it next tries to send.
pub async fn explain_stream(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
    body: Option<Json<ExplainStreamRequest>>,
) -> impl IntoResponse {
    let mut req = body.map(|Json(b)| b).unwrap_or_default();
    // Default the backend to the server's active target when the request omits
    // one, so switching to Copilot via /api/llm/target actually reaches explain
    // — rather than always falling back to the OpenAI default downstream.
    if req.llm.is_none() {
        req.llm = Some(state.container.llm_target().into());
    }
    // The call-graph query filters on the repository UUID, so a repository
    // NAME must be resolved first — passing it through unresolved silently
    // matches nothing and the explanation aborts with "no callers or callees".
    if let Some(repo) = req.repository.take() {
        req.repository = Some(state.container.resolve_repository_id(Some(&repo)).await);
    }
    // Build the use case up front so the spawned task owns only what it needs;
    // the `Arc<Container>` can then drop when this handler returns rather than
    // staying alive for the whole (potentially long) SSE stream. Snippets are
    // scoped to the repository's own namespace — the boot namespace's chunk
    // schema knows nothing about repos indexed elsewhere.
    let use_case = state
        .container
        .explain_use_case_for_repository(req.repository.as_deref())
        .await;
    // Copilot reads its token/model from `<data_dir>/config.json`; capture the
    // path now so the spawned task doesn't need to hold the container alive.
    let data_dir = state.container.data_dir().to_string();

    // Cache is only used for a repository-scoped request (the key needs a single
    // repository id) that isn't a regex match (the resolved symbol is stable).
    // The key's model component identifies the effective backend+model so a
    // different model recomputes rather than reusing another's output.
    let cache = (!req.regex)
        .then(|| req.repository.clone())
        .flatten()
        .map(|repo_id| ExplanationCacheKey {
            repository_id: repo_id,
            symbol: symbol.clone(),
            model: explanation_model_key(&state, &req, &data_dir),
        });
    let analysis_repo = state.container.analysis_repository();

    // Channel of ready-to-send SSE events feeding the HTTP response.
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    tokio::spawn(async move {
        run_explain_stream(
            use_case,
            symbol,
            req,
            data_dir,
            analysis_repo,
            cache,
            event_tx,
        )
        .await;
    });

    Sse::new(receiver_stream(event_rx)).keep_alive(KeepAlive::default())
}

/// The `(repository, symbol, model)` cache key for an explanation.
struct ExplanationCacheKey {
    repository_id: String,
    symbol: String,
    model: String,
}

/// A stable identifier of the backend+model that will answer this request, used
/// as the cache key's model component. Combines the resolved target with the
/// effective model (the request override, else the backend's configured model)
/// so switching either produces a distinct cache entry.
fn explanation_model_key(state: &AppState, req: &ExplainStreamRequest, data_dir: &str) -> String {
    let target: LlmTarget = req
        .llm
        .map(Into::into)
        .unwrap_or_else(|| state.container.llm_target());
    let model = req.model.clone().unwrap_or_else(|| match target {
        LlmTarget::Copilot => CodesearchConfig::load(data_dir)
            .ok()
            .and_then(|c| c.copilot.and_then(|cp| cp.model))
            .unwrap_or_default(),
        LlmTarget::OpenAi => CodesearchConfig::load(data_dir)
            .ok()
            .and_then(|c| c.resolve_openai_endpoint(req.endpoint.as_deref()))
            .and_then(|e| e.model)
            .unwrap_or_default(),
        LlmTarget::Anthropic => String::new(),
    });
    format!("{}:{}", target.as_str(), model)
}

/// Drive the explain use case and forward its output as SSE events on `event_tx`.
async fn run_explain_stream(
    use_case: crate::application::ExplainUseCase,
    symbol: String,
    req: ExplainStreamRequest,
    data_dir: String,
    analysis_repo: Arc<dyn crate::application::AnalysisRepository>,
    cache: Option<ExplanationCacheKey>,
    event_tx: mpsc::UnboundedSender<Event>,
) {
    // Cache hit: replay the stored explanation as token frames + a done event so
    // the client renders it identically to a live run, without any LLM call.
    // Skipped when regenerating (the user asked for a fresh answer).
    if !req.regenerate {
        if let Some(key) = &cache {
            if let Ok(Some(cached)) = analysis_repo
                .get_cached_explanation(&key.repository_id, &key.symbol, &key.model)
                .await
            {
                let _ = event_tx.send(sse_event("token", json!({ "text": cached })));
                let _ = event_tx.send(sse_event(
                    "done",
                    json!({
                        "status": "ok",
                        "root_symbol": symbol,
                        "explanation": cached,
                        "cached": true,
                    }),
                ));
                return;
            }
        }
    }

    let llm: LlmTarget = req.llm.map(Into::into).unwrap_or_default();
    let chat_client: Arc<dyn ChatClient> = match llm {
        LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
        LlmTarget::OpenAi => {
            match OpenAiChatClient::from_config(&data_dir, req.endpoint.as_deref()) {
                Ok(client) => Arc::new(client),
                Err(err) => {
                    let _ = event_tx.send(sse_event(
                        "error",
                        json!({ "message": format!("failed to initialise OpenAI client: {err}") }),
                    ));
                    return;
                }
            }
        }
        LlmTarget::Copilot => {
            match CopilotChatClient::from_data_dir_with_model(&data_dir, req.model) {
                Ok(client) => Arc::new(client),
                Err(err) => {
                    let _ = event_tx.send(sse_event(
                        "error",
                        json!({ "message": format!("failed to initialise Copilot client: {err}") }),
                    ));
                    return;
                }
            }
        }
    };

    // Per-token channel from the use case; we relay each token as an SSE frame.
    let (token_tx, mut token_rx) = mpsc::unbounded_channel::<String>();

    let symbol_c = symbol.clone();
    let repo_c = req.repository.clone();
    let is_regex = req.regex;
    let client_c = chat_client.clone();

    let work = tokio::spawn(async move {
        use_case
            .execute_streaming(
                &symbol_c,
                repo_c.as_deref(),
                client_c.as_ref(),
                is_regex,
                token_tx,
            )
            .await
    });

    // Relay tokens as they arrive. `send` fails once the client disconnects
    // (the SSE stream's receiver is dropped); we stop early in that case.
    while let Some(token) = token_rx.recv().await {
        if event_tx
            .send(sse_event("token", json!({ "text": token })))
            .is_err()
        {
            work.abort();
            return;
        }
    }

    // Token stream drained — collect the final result.
    match work.await {
        Ok(Ok(result)) => {
            if !result.ambiguous_candidates.is_empty() {
                let _ = event_tx.send(sse_event(
                    "done",
                    json!({
                        "status": "ambiguous",
                        "root_symbol": result.root_symbol,
                        "candidates": result.ambiguous_candidates,
                        "is_regex": result.is_regex,
                    }),
                ));
                return;
            }
            // Store the fresh explanation so the next identical request is served
            // from cache (0 LLM tokens). A Regenerate overwrites the prior entry.
            // Best-effort: a cache write failure must not fail the response the
            // user already received.
            if !result.explanation.is_empty() {
                if let Some(key) = &cache {
                    if let Err(e) = analysis_repo
                        .save_explanation(
                            &key.repository_id,
                            &key.symbol,
                            &key.model,
                            &result.explanation,
                        )
                        .await
                    {
                        tracing::warn!("failed to cache explanation for {}: {e}", key.symbol);
                    }
                }
            }
            let referenced: Vec<serde_json::Value> = result
                .symbol_sources
                .iter()
                .map(|(sym, repo, file, line, _src)| {
                    json!({
                        "symbol": sym,
                        "repository": repo,
                        "file": file,
                        "line": line,
                    })
                })
                .collect();
            let _ = event_tx.send(sse_event(
                "done",
                json!({
                    "status": "ok",
                    "root_symbol": result.root_symbol,
                    "explanation": result.explanation,
                    "total_affected": result.total_affected,
                    "max_depth_reached": result.max_depth_reached,
                    "referenced": referenced,
                    "cached": false,
                }),
            ));
        }
        Ok(Err(err)) => {
            let _ = event_tx.send(sse_event(
                "error",
                json!({ "message": format!("explain failed: {err}") }),
            ));
        }
        Err(join_err) => {
            let _ = event_tx.send(sse_event(
                "error",
                json!({ "message": format!("explain task panicked: {join_err}") }),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/stream/index
// ---------------------------------------------------------------------------

/// `POST /api/stream/index` — stream indexing progress for a repository path.
///
/// The underlying `index_repository` use case renders its own internal progress
/// bar and does not (yet) expose a fine-grained progress channel, so this
/// endpoint emits **coarse** lifecycle `progress` events (`started` → `running`
/// heartbeats → terminal) rather than per-file counters. When the use case
/// finishes it emits a single `done` event with the repository summary, or an
/// `error` event on failure. See the module docs for the exact event schema.
pub async fn index_stream(
    State(state): State<AppState>,
    Json(req): Json<IndexStreamRequest>,
) -> impl IntoResponse {
    let container = state.container.clone();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    tokio::spawn(async move {
        run_index_stream(container, req, event_tx).await;
    });

    Sse::new(receiver_stream(event_rx)).keep_alive(KeepAlive::default())
}

/// Heartbeat cadence for the coarse index `running` progress events.
const INDEX_HEARTBEAT_SECS: u64 = 2;

/// Drive the index use case and forward coarse progress as SSE events.
async fn run_index_stream(
    container: Arc<crate::connector::api::Container>,
    req: IndexStreamRequest,
    event_tx: mpsc::UnboundedSender<Event>,
) {
    let _ = event_tx.send(sse_event(
        "progress",
        json!({ "stage": "started", "message": format!("indexing {}", req.path) }),
    ));

    let (store, namespace): (VectorStore, Option<String>) = if container.memory_storage() {
        (VectorStore::InMemory, None)
    } else {
        (VectorStore::DuckDb, Some(container.namespace().to_string()))
    };

    let use_case = container.index_use_case();
    let path = req.path.clone();
    let name = req.name.clone();
    let force = req.force;

    // Run the (self-contained) indexing work in a task so we can interleave
    // periodic "running" heartbeats while it proceeds.
    let mut work = tokio::spawn(async move {
        use_case
            .execute(&path, name.as_deref(), store, namespace, force)
            .await
    });

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(INDEX_HEARTBEAT_SECS));
    heartbeat.tick().await; // first tick is immediate — skip it

    loop {
        tokio::select! {
            result = &mut work => {
                match result {
                    Ok(Ok(repo)) => {
                        let languages: Vec<serde_json::Value> = repo
                            .languages()
                            .iter()
                            .map(|(lang, stats)| json!({
                                "language": lang.to_string(),
                                "files": stats.file_count,
                            }))
                            .collect();
                        let _ = event_tx.send(sse_event(
                            "done",
                            json!({
                                "status": "ok",
                                "name": repo.name(),
                                "file_count": repo.file_count(),
                                "chunk_count": repo.chunk_count(),
                                "languages": languages,
                            }),
                        ));
                    }
                    Ok(Err(err)) => {
                        let _ = event_tx.send(sse_event(
                            "error",
                            json!({ "message": format!("indexing failed: {err}") }),
                        ));
                    }
                    Err(join_err) => {
                        let _ = event_tx.send(sse_event(
                            "error",
                            json!({ "message": format!("indexing task panicked: {join_err}") }),
                        ));
                    }
                }
                return;
            }
            _ = heartbeat.tick() => {
                if event_tx.send(sse_event(
                    "progress",
                    json!({ "stage": "running", "message": "indexing in progress" }),
                )).is_err() {
                    // Client gone — drop the work and stop.
                    work.abort();
                    return;
                }
            }
        }
    }
}
