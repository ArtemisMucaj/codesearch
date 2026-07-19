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

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;

use crate::connector::adapter::{copilot_auth, CodesearchConfig};

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

/// Shared Copilot-login state for serve mode. One attempt runs at a time; the
/// background poll updates `status`, which the GET endpoint reads.
pub struct CopilotLoginService {
    data_dir: String,
    status: Arc<Mutex<LoginStatus>>,
}

impl CopilotLoginService {
    pub fn new(data_dir: String) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            status: Arc::new(Mutex::new(LoginStatus::Idle)),
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
    /// A restart while a previous attempt is still pending simply supersedes it:
    /// the new device code becomes the tracked one (the old poll task's result
    /// is ignored because it writes the same shared status, which the new task
    /// overwrites — harmless, and the user only ever sees the latest code).
    pub async fn start(self: &Arc<Self>) -> LoginStatus {
        let http = reqwest::Client::new();
        let device = match copilot_auth::request_device_code(&http).await {
            Ok(d) => d,
            Err(e) => {
                let failed = LoginStatus::Failed {
                    error: format!("failed to start GitHub device-flow login: {e}"),
                };
                *self.status.lock().await = failed.clone();
                return failed;
            }
        };

        let pending = LoginStatus::Pending {
            user_code: device.user_code().to_string(),
            verification_uri: device.verification_uri().to_string(),
        };
        *self.status.lock().await = pending.clone();

        // Poll + persist in the background so the request returns immediately.
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let result = copilot_auth::poll_for_token(&http, &device).await;
            let next = match result {
                Ok(token) => match service.persist_token(token) {
                    Ok(()) => LoginStatus::Authorized,
                    Err(e) => LoginStatus::Failed {
                        error: format!("login succeeded but saving the token failed: {e}"),
                    },
                },
                Err(e) => LoginStatus::Failed {
                    error: format!("GitHub device-flow login failed: {e}"),
                },
            };
            *service.status.lock().await = next;
        });

        pending
    }

    /// Persist the `ghu_…` token into `config.json`'s copilot section, exactly
    /// as `codesearch copilot login` does.
    fn persist_token(&self, token: String) -> Result<(), crate::domain::DomainError> {
        let mut cfg = CodesearchConfig::load(&self.data_dir)?;
        cfg.copilot_mut().github_token = Some(token);
        cfg.save(&self.data_dir)
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
        assert_eq!(pending["verification_uri"], "https://github.com/login/device");

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
