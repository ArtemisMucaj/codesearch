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
//! On success the `ghu_…` token is persisted into `config.json` exactly as the
//! CLI does, so every other Copilot path (models, chat) picks it up.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::warn;

use crate::connector::adapter::{copilot_auth, CodesearchConfig};
use crate::domain::DomainError;

/// The current state of a Copilot login attempt, serialized `snake_case`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginStatus {
    /// No login has been started this session.
    Idle,
    /// A device code was issued; waiting for the user to authorize in a browser.
    Pending {
        user_code: String,
        verification_uri: String,
    },
    /// The user authorized and the token was stored.
    Authorized,
    /// The flow failed (denied, expired, or a network error); carries a reason.
    Failed { error: String },
}

/// Shared Copilot-login state for serve mode. One attempt is tracked at a time;
/// the background poll updates `status`, which the GET endpoint reads.
pub struct CopilotLoginService {
    data_dir: String,
    status: Arc<Mutex<LoginStatus>>,
    /// Monotonic id of the current attempt. `start` bumps it; a background poll
    /// only writes `status` if its id still matches — so a superseded attempt
    /// (the user restarted) can never clobber the newer one's result.
    generation: Arc<AtomicU64>,
}

impl CopilotLoginService {
    pub fn new(data_dir: String) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            status: Arc::new(Mutex::new(LoginStatus::Idle)),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    /// The current status, for `GET /api/llm/copilot/login`.
    pub async fn status(&self) -> LoginStatus {
        self.status.lock().await.clone()
    }

    /// Start (or restart) the device flow. Requests a device code synchronously
    /// so the caller gets the `user_code` immediately, then spawns a background
    /// task that polls for the token and persists it. Returns the `Pending`
    /// status (or `Failed` if the device-code request itself failed).
    ///
    /// Restarting supersedes any in-flight attempt: `start` bumps a generation
    /// id, and a background poll only writes its result if the id still matches,
    /// so a stale attempt can never overwrite the newer one's status.
    pub async fn start(self: &Arc<Self>) -> LoginStatus {
        // Claim this attempt; any older poll task's writes are now ignored.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        let http = reqwest::Client::new();
        let device = match copilot_auth::request_device_code(&http).await {
            Ok(d) => d,
            Err(e) => {
                warn!("copilot login: device-code request failed: {e}");
                let failed = LoginStatus::Failed {
                    error: format!("failed to start GitHub device-flow login: {e}"),
                };
                self.set_status(generation, failed.clone()).await;
                return failed;
            }
        };

        let pending = LoginStatus::Pending {
            user_code: device.user_code().to_string(),
            verification_uri: device.verification_uri().to_string(),
        };
        self.set_status(generation, pending.clone()).await;

        // Poll + persist in the background so the request returns immediately.
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let next = match copilot_auth::poll_for_token(&http, &device).await {
                Ok(token) => match service.persist_token(token).await {
                    Ok(()) => LoginStatus::Authorized,
                    Err(e) => {
                        warn!("copilot login: token saved-but-failed: {e}");
                        LoginStatus::Failed {
                            error: format!("login succeeded but saving the token failed: {e}"),
                        }
                    }
                },
                Err(e) => {
                    warn!("copilot login: device-flow poll failed: {e}");
                    LoginStatus::Failed {
                        error: format!("GitHub device-flow login failed: {e}"),
                    }
                }
            };
            service.set_status(generation, next).await;
        });

        pending
    }

    /// Write `status` only if `generation` is still the current attempt — so a
    /// superseded poll task's terminal result is dropped instead of clobbering
    /// a newer attempt the user has since started.
    async fn set_status(&self, generation: u64, status: LoginStatus) {
        if self.generation.load(Ordering::SeqCst) == generation {
            *self.status.lock().await = status;
        }
    }

    /// Persist the `ghu_…` token into `config.json`'s copilot section, exactly
    /// as `codesearch copilot login` does. The config read/write is blocking
    /// filesystem I/O, so it runs on `spawn_blocking`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The status serializes with a `status` discriminator + snake_case values,
    /// which is the shape the client decodes.
    #[test]
    fn login_status_serializes_with_tag() {
        let idle = serde_json::to_value(LoginStatus::Idle).unwrap();
        assert_eq!(idle["status"], "idle");

        let pending = serde_json::to_value(LoginStatus::Pending {
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
        })
        .unwrap();
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["user_code"], "ABCD-1234");
        assert_eq!(
            pending["verification_uri"],
            "https://github.com/login/device"
        );

        assert_eq!(
            serde_json::to_value(LoginStatus::Authorized).unwrap()["status"],
            "authorized"
        );

        let failed = serde_json::to_value(LoginStatus::Failed {
            error: "denied".into(),
        })
        .unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"], "denied");
    }
}
