//! GitHub Copilot device-flow login for `codesearch serve`.
//!
//! The native app bundles the `codesearch` binary but does not put it on the
//! user's PATH, so `codesearch copilot login` (an interactive terminal command)
//! isn't runnable. This service exposes the same OAuth **device flow** over the
//! management API so a GUI can drive it:
//!
//! 1. `POST /api/llm/copilot/login` requests a device code and returns the
//!    `user_code` + `verification_uri` to show the user, then polls GitHub in
//!    the background until the user authorizes (or the code expires).
//! 2. `GET /api/llm/copilot/login` reports the current [`LoginStatus`] so the UI
//!    can advance from *pending* to *authorized* / *failed*.
//!
//! The device flow and its status machine come from
//! [`gh_copilot_rs::LoginSession`]; this wrapper adds the codesearch-specific
//! step — persisting the `ghu_…` token into `config.json` on success, exactly as
//! the CLI does, so every other Copilot path (models, chat) picks it up. The
//! serialized [`LoginStatus`] shape is unchanged, so the HTTP contract the
//! native app depends on is preserved.

use std::sync::Arc;

use gh_copilot_rs::{GitHubDeviceFlow, LoginSession, LoginStatus};
use tracing::warn;

use crate::connector::adapter::CodesearchConfig;
use crate::domain::DomainError;

/// Shared Copilot-login state for serve mode, wrapping a [`LoginSession`] and
/// persisting the token to `config.json` once the session reports `authorized`.
pub struct CopilotLoginService {
    data_dir: String,
    session: Arc<LoginSession>,
}

impl CopilotLoginService {
    pub fn new(data_dir: String) -> Arc<Self> {
        // A failed device-flow client build leaves the session unusable; fall
        // back to a session that will simply report `failed` on start rather
        // than panicking at construction (serve must still boot).
        let flow = GitHubDeviceFlow::new()
            .map(|f| Arc::new(f) as Arc<dyn gh_copilot_rs::DeviceFlow>)
            .unwrap_or_else(|e| {
                warn!("copilot login: device-flow client unavailable: {e}");
                Arc::new(UnavailableFlow(e.to_string()))
            });
        Arc::new(Self {
            data_dir,
            session: LoginSession::new(flow),
        })
    }

    /// The current status, for `GET /api/llm/copilot/login`.
    pub async fn status(&self) -> LoginStatus {
        self.session.status().await
    }

    /// Start (or restart) the device flow. Returns the initial status (`Pending`
    /// with the code to display, or `Failed`) immediately; a background task
    /// waits for the session to authorize and then persists the token.
    pub async fn start(self: &Arc<Self>) -> LoginStatus {
        let status = self.session.start().await;

        // If the flow started, wait for it to finish in the background and
        // persist the token the moment the session reports success. The session
        // itself no longer writes to disk, so persistence lives here.
        if matches!(status, LoginStatus::Pending { .. }) {
            let service = Arc::clone(self);
            tokio::spawn(async move {
                service.persist_on_success().await;
            });
        }

        status
    }

    /// Poll the session until it leaves `pending`; on `authorized`, read the
    /// token and write it to `config.json`.
    async fn persist_on_success(&self) {
        loop {
            match self.session.status().await {
                LoginStatus::Pending { .. } => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                LoginStatus::Authorized => {
                    if let Some(token) = self.session.token().await {
                        if let Err(e) = self.persist_token(token.expose().to_string()).await {
                            warn!("copilot login: token saved-but-failed: {e}");
                        }
                    }
                    return;
                }
                // Failed or Idle (superseded): nothing to persist.
                _ => return,
            }
        }
    }

    /// Persist the `ghu_…` token into `config.json`'s copilot section. The
    /// config read/write is blocking filesystem I/O, so it runs on
    /// `spawn_blocking`.
    async fn persist_token(&self, token: String) -> Result<(), DomainError> {
        let data_dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<(), DomainError> {
            let mut cfg = CodesearchConfig::load(&data_dir)?;
            cfg.copilot_mut().github_token = Some(token);
            cfg.save(&data_dir)
        })
        .await
        .map_err(|e| DomainError::internal(format!("token persist task panicked: {e}")))?
    }
}

/// A [`DeviceFlow`](gh_copilot_rs::DeviceFlow) that fails every call — used when
/// the real client could not be built, so a login attempt reports `failed`
/// instead of taking the whole server down at boot.
struct UnavailableFlow(String);

#[async_trait::async_trait]
impl gh_copilot_rs::DeviceFlow for UnavailableFlow {
    async fn request_device_code(
        &self,
    ) -> Result<gh_copilot_rs::DeviceAuthorization, gh_copilot_rs::CopilotError> {
        Err(gh_copilot_rs::CopilotError::configuration(self.0.clone()))
    }

    async fn poll_once(
        &self,
        _authorization: &gh_copilot_rs::DeviceAuthorization,
    ) -> Result<gh_copilot_rs::PollOutcome, gh_copilot_rs::CopilotError> {
        Err(gh_copilot_rs::CopilotError::configuration(self.0.clone()))
    }
}
