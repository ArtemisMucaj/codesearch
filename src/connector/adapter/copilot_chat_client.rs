//! [`ChatClient`] backed by a **GitHub Copilot subscription**.
//!
//! Unlike [`OpenAiChatClient`](super::OpenAiChatClient) /
//! [`AnthropicClient`](super::AnthropicClient), which speak raw HTTP to an
//! OpenAI/Anthropic-compatible endpoint, this adapter drives the official
//! `copilot` CLI over JSON-RPC via the [`github_copilot_sdk`] crate. The CLI
//! owns the two-layer Copilot auth (long-lived GitHub OAuth token → short-lived
//! Copilot JWT) and refreshes tokens transparently, so we never handle the
//! reverse-engineered token-exchange endpoint ourselves.
//!
//! ## Auth
//!
//! The GitHub OAuth token (`ghu_…`) captured by `codesearch copilot login` is
//! read from `<data_dir>/config.json` and handed to the CLI. When no token is
//! stored, the CLI falls back to its own device-flow login on first use.
//!
//! ## Mapping the [`ChatClient`] contract onto a chat session
//!
//! The SDK models a turn as: open a session, `send` a user prompt, and consume
//! `assistant.message_delta` / `assistant.message` events. Our contract is a
//! single `system` + `user` → text call, so for each completion we:
//!
//! 1. open a fresh session with the `system` prompt installed as a
//!    [`SystemMessageConfig`] (mode `replace`) and permissions auto-approved
//!    (the search prompts never touch tools, but a stray request must not
//!    deadlock on a missing handler);
//! 2. subscribe to session events and drive the turn with `send_and_wait`;
//! 3. return the final `assistant.message` content (falling back to the
//!    concatenation of streamed deltas if no terminal message arrives).
//!
//! The underlying [`Client`] (which spawns the CLI child process) is created
//! once and shared across calls; only the lightweight session is per-call.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use github_copilot_sdk::types::{MessageOptions, SessionConfig, SystemMessageConfig};
use github_copilot_sdk::{Client, ClientOptions, Model};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::connector::adapter::ChatClient;
use crate::domain::DomainError;

/// Wall-clock budget for a single completion turn. Matches the generous
/// timeout the HTTP chat clients use, so slow reasoning models are not cut off.
const TURN_TIMEOUT: Duration = Duration::from_secs(300);

/// Wall-clock budget for listing models (spawns the CLI + one RPC). Short so a
/// stalled CLI can't hang the `/api/llm/models` request or the login picker.
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(30);

/// SDK event `type` for the final assistant message (carries the full text).
const EVENT_ASSISTANT_MESSAGE: &str = "assistant.message";
/// SDK event `type` for an incremental streamed token chunk.
const EVENT_ASSISTANT_MESSAGE_DELTA: &str = "assistant.message_delta";
/// SDK event `type` for a session-level error.
const EVENT_SESSION_ERROR: &str = "session.error";

/// [`ChatClient`] that routes completions through a GitHub Copilot
/// subscription via the `copilot` CLI.
/// Sub-directory of `data_dir` used as the Copilot CLI's `COPILOT_HOME` so its
/// credentials/state live alongside (but isolated from) the rest of codesearch.
pub const COPILOT_HOME_SUBDIR: &str = "copilot";

pub struct CopilotChatClient {
    /// Lazily-started SDK client (spawns the CLI child on first use). Shared by
    /// every completion so we pay the process-spawn cost once.
    client: OnceCell<Arc<Client>>,
    /// GitHub OAuth token handed to the CLI; `None` lets the CLI use its own
    /// stored credentials (under [`Self::copilot_home`]) or the logged-in user.
    github_token: Option<String>,
    /// Model id to request (e.g. `"claude-sonnet-4.5"`); `None` uses the CLI
    /// default.
    model: Option<String>,
    /// `COPILOT_HOME` for the CLI child — where it persists auth/session state.
    /// `None` uses the CLI's own default location.
    copilot_home: Option<std::path::PathBuf>,
}

impl CopilotChatClient {
    /// Construct a client with an explicit token and model.
    pub fn new(github_token: Option<String>, model: Option<String>) -> Self {
        Self {
            client: OnceCell::new(),
            github_token,
            model,
            copilot_home: None,
        }
    }

    /// Point the CLI's persisted state (`COPILOT_HOME`) at `dir`. Used so the
    /// login command and later chat calls share one isolated credential store.
    pub fn with_copilot_home(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.copilot_home = Some(dir.into());
        self
    }

    /// Build a client from persisted configuration under `data_dir`
    /// (`<data_dir>/config.json`), with `COPILOT_HOME` at `<data_dir>/copilot`.
    /// A missing file / missing Copilot section yields a client with no stored
    /// token (CLI uses its own credentials) and the default model.
    pub fn from_data_dir(data_dir: &str) -> Result<Self, DomainError> {
        Self::from_data_dir_with_model(data_dir, None)
    }

    /// Like [`Self::from_data_dir`] but applies a per-call model override on top
    /// of the stored selection when `model_override` is `Some` — the path used
    /// by serve-mode requests that pick a model on the fly.
    pub fn from_data_dir_with_model(
        data_dir: &str,
        model_override: Option<String>,
    ) -> Result<Self, DomainError> {
        let copilot = super::CodesearchConfig::load_copilot(data_dir)?;
        let model = model_override.or(copilot.model);
        Ok(Self::new(copilot.github_token, model)
            .with_copilot_home(std::path::Path::new(data_dir).join(COPILOT_HOME_SUBDIR)))
    }

    /// The model id this client is configured to request, if any (for logging).
    pub fn configured_model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Report whether the CLI considers itself authenticated (drives the login
    /// command's status output and its "already logged in?" check).
    pub async fn auth_status(
        &self,
    ) -> Result<github_copilot_sdk::types::GetAuthStatusResponse, DomainError> {
        let client = self.client().await?;
        client
            .get_auth_status()
            .await
            .map_err(|e| DomainError::internal(format!("failed to query Copilot auth status: {e}")))
    }

    /// Assemble the [`ClientOptions`], injecting the stored token and the
    /// isolated `COPILOT_HOME` when configured.
    fn client_options(&self) -> ClientOptions {
        let mut opts = ClientOptions::default();
        if let Some(token) = self.github_token.as_ref().filter(|t| !t.is_empty()) {
            opts = opts.with_github_token(token.clone());
        }
        if let Some(home) = &self.copilot_home {
            opts = opts.with_base_directory(home.clone());
        }
        opts
    }

    /// Start (once) and return the shared SDK client.
    async fn client(&self) -> Result<Arc<Client>, DomainError> {
        self.client
            .get_or_try_init(|| async {
                debug!(
                    "CopilotChatClient: starting Copilot CLI (model={:?}, token={})",
                    self.model,
                    if self.github_token.is_some() {
                        "stored"
                    } else {
                        "cli-login"
                    }
                );
                Client::start(self.client_options())
                    .await
                    .map(Arc::new)
                    .map_err(|e| DomainError::internal(format!("failed to start Copilot CLI: {e}")))
            })
            .await
            .cloned()
    }

    /// List the models available to the authenticated Copilot account. Backs
    /// `codesearch copilot models`, the login-TUI picker, and the serve-mode
    /// `GET /api/llm/models` endpoint.
    ///
    /// Bounded by [`LIST_MODELS_TIMEOUT`] so a stalled CLI/SDK call can't hang
    /// the caller (notably the `/api/llm/models` HTTP request) indefinitely.
    pub async fn list_models(&self) -> Result<Vec<Model>, DomainError> {
        let fetch = async {
            let client = self.client().await?;
            client
                .list_models()
                .await
                .map_err(|e| DomainError::internal(format!("failed to list Copilot models: {e}")))
        };
        tokio::time::timeout(LIST_MODELS_TIMEOUT, fetch)
            .await
            .map_err(|_| {
                DomainError::internal(format!(
                    "listing Copilot models timed out after {}s",
                    LIST_MODELS_TIMEOUT.as_secs()
                ))
            })?
    }

    /// Build the per-turn [`SessionConfig`]: system prompt, model, streaming
    /// flag, and denied tool permissions.
    fn session_config(&self, system: &str, streaming: bool) -> SessionConfig {
        // These are one-shot text prompts (query expansion, explain, naming);
        // they never legitimately invoke tools. Deny every tool request so the
        // session fails closed — a stray/injected tool call can't take a side
        // effect — rather than auto-approving everything.
        let mut cfg = SessionConfig::default().deny_all_permissions();
        cfg.streaming = Some(streaming);
        // Install the caller's system prompt verbatim. `replace` (rather than
        // the default `append`) keeps the CLI's own agent preamble out of these
        // tightly-scoped search prompts.
        cfg.system_message = Some(
            SystemMessageConfig::new()
                .with_mode("replace")
                .with_content(system),
        );
        if let Some(model) = &self.model {
            cfg = cfg.with_model(model.clone());
        }
        cfg
    }

    /// Run one turn: open a session, send `user`, and collect the assistant's
    /// reply. When `token_tx` is `Some`, each streamed delta is forwarded to it
    /// as it arrives.
    async fn run_turn(
        &self,
        system: &str,
        user: &str,
        token_tx: Option<UnboundedSender<String>>,
    ) -> Result<String, DomainError> {
        let client = self.client().await?;
        let streaming = token_tx.is_some();
        let session = client
            .create_session(self.session_config(system, streaming))
            .await
            .map_err(|e| DomainError::internal(format!("failed to create Copilot session: {e}")))?;

        // Collect assistant output on a background task while `send_and_wait`
        // drives the turn. Deltas accumulate into `full_text`; a terminal
        // `assistant.message` supersedes them with the authoritative content.
        let mut events = session.subscribe();
        let collector = tokio::spawn(async move {
            let mut streamed = String::new();
            let mut final_text: Option<String> = None;
            let mut error: Option<String> = None;
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    // The stream ended (Closed) or we fell behind (Lagged)
                    // before a terminal `assistant.message`. Record it as an
                    // error: returning the partial `streamed` text as a success
                    // would make a truncated answer indistinguishable from a
                    // complete one.
                    Err(e) => {
                        error = Some(format!("event stream ended before completion: {e}"));
                        break;
                    }
                };
                match event.event_type.as_str() {
                    EVENT_ASSISTANT_MESSAGE_DELTA => {
                        if let Some(delta) = event.data.get("deltaContent").and_then(|v| v.as_str())
                        {
                            streamed.push_str(delta);
                            if let Some(tx) = &token_tx {
                                // Receiver gone (client disconnected) → stop
                                // forwarding; the turn still completes.
                                let _ = tx.send(delta.to_string());
                            }
                        }
                    }
                    EVENT_ASSISTANT_MESSAGE => {
                        if let Some(content) = event.data.get("content").and_then(|v| v.as_str()) {
                            final_text = Some(content.to_string());
                        }
                        // One assistant message per turn — we're done listening.
                        break;
                    }
                    EVENT_SESSION_ERROR => {
                        error = Some(
                            event
                                .data
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown Copilot session error")
                                .to_string(),
                        );
                        break;
                    }
                    _ => {}
                }
            }
            (final_text.unwrap_or(streamed), error)
        });

        let send_result = session
            .send_and_wait(MessageOptions::new(user).with_wait_timeout(TURN_TIMEOUT))
            .await;

        // Always tear the session down, regardless of how the turn went.
        if let Err(e) = session.disconnect().await {
            warn!("CopilotChatClient: failed to disconnect session: {e}");
        }

        // Surface a send/timeout failure ahead of whatever the collector saw.
        if let Err(e) = send_result {
            collector.abort();
            return Err(DomainError::internal(format!("Copilot turn failed: {e}")));
        }

        let (text, error) = collector
            .await
            .map_err(|e| DomainError::internal(format!("Copilot event collector panicked: {e}")))?;

        if let Some(msg) = error {
            return Err(DomainError::internal(format!("Copilot error: {msg}")));
        }
        if text.trim().is_empty() {
            return Err(DomainError::internal("Copilot returned an empty response"));
        }
        Ok(text)
    }
}

#[async_trait]
impl ChatClient for CopilotChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        self.run_turn(system, user, None).await
    }

    async fn complete_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        self.run_turn(system, user, Some(token_tx)).await
    }
}
