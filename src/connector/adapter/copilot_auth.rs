//! GitHub OAuth **device flow** for the Copilot backend.
//!
//! codesearch talks to the Copilot API directly over HTTP, so it performs the
//! OAuth device flow itself rather than delegating to an external CLI. The flow
//! (per [RFC 8628]) is:
//!
//! 1. `POST https://github.com/login/device/code` → a `user_code` to type and a
//!    `verification_uri` to open in a browser.
//! 2. Poll `POST https://github.com/login/oauth/access_token` with the
//!    `device_code` until the user completes the browser step, honoring the
//!    server's `interval` and `slow_down` back-pressure.
//!
//! The resulting `ghu_…` token is a long-lived GitHub OAuth token that the
//! Copilot API accepts directly as a `Bearer` credential (no separate
//! `/copilot_internal/v2/token` exchange is required).
//!
//! [RFC 8628]: https://www.rfc-editor.org/rfc/rfc8628

use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

use crate::domain::DomainError;

/// Public GitHub OAuth client id of the VS Code Copilot Chat extension. It is
/// the client id community Copilot integrations use for the device flow; GitHub
/// issues Copilot-capable tokens for it. Not a secret (device-flow public
/// clients have none).
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// Scope requested for the token. `read:user` is what the Copilot device flow
/// grants against; the Copilot entitlement rides on the account, not the scope.
const SCOPE: &str = "read:user";

/// Small buffer added to each poll interval to absorb clock skew / timer drift
/// so we never poll slightly too early and trip `slow_down`.
const POLL_SAFETY_MARGIN: Duration = Duration::from_secs(1);

/// The device-code grant, ready to display to the user and poll on.
pub struct DeviceCode {
    /// Code the user types into the verification page.
    pub user_code: String,
    /// URL the user opens to enter the code.
    pub verification_uri: String,
    /// Opaque code we poll the token endpoint with.
    device_code: String,
    /// Seconds the server asks us to wait between polls.
    interval: u64,
}

impl DeviceCode {
    pub fn user_code(&self) -> &str {
        &self.user_code
    }
    pub fn verification_uri(&self) -> &str {
        &self.verification_uri
    }
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    /// Some `slow_down` responses carry a new interval to adopt.
    interval: Option<u64>,
}

/// Step 1: request a device code from GitHub.
pub async fn request_device_code(client: &reqwest::Client) -> Result<DeviceCode, DomainError> {
    let resp = client
        .post(DEVICE_CODE_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&serde_json::json!({ "client_id": CLIENT_ID, "scope": SCOPE }))
        .send()
        .await
        .map_err(|e| DomainError::internal(format!("device-code request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(DomainError::internal(format!(
            "device-code request returned {status}: {body}"
        )));
    }

    let data: DeviceCodeResponse = resp
        .json()
        .await
        .map_err(|e| DomainError::internal(format!("failed to parse device-code response: {e}")))?;

    Ok(DeviceCode {
        user_code: data.user_code,
        verification_uri: data.verification_uri,
        device_code: data.device_code,
        interval: data.interval,
    })
}

/// Step 2: poll the token endpoint until the user authorizes (or it fails).
///
/// Blocks — honoring the server's `interval` / `slow_down` — until a token is
/// issued, then returns the `ghu_…` access token. Returns an error if GitHub
/// reports `access_denied`, `expired_token`, or any other terminal error.
pub async fn poll_for_token(
    client: &reqwest::Client,
    device: &DeviceCode,
) -> Result<String, DomainError> {
    let mut interval = Duration::from_secs(device.interval);
    loop {
        tokio::time::sleep(interval + POLL_SAFETY_MARGIN).await;

        let resp = client
            .post(ACCESS_TOKEN_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "client_id": CLIENT_ID,
                "device_code": device.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .send()
            .await
            .map_err(|e| DomainError::internal(format!("token poll request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DomainError::internal(format!(
                "token poll returned {status}: {body}"
            )));
        }

        let data: AccessTokenResponse = resp
            .json()
            .await
            .map_err(|e| DomainError::internal(format!("failed to parse token response: {e}")))?;

        if let Some(token) = data.access_token {
            return Ok(token);
        }

        match data.error.as_deref() {
            // Still waiting on the user — keep polling at the current cadence.
            Some("authorization_pending") => {
                debug!("copilot login: authorization pending, still polling");
            }
            // We polled too fast; adopt the new interval (or bump by 5s per RFC).
            Some("slow_down") => {
                interval = data
                    .interval
                    .map(Duration::from_secs)
                    .unwrap_or(interval + Duration::from_secs(5));
                debug!("copilot login: slow_down, new interval {interval:?}");
            }
            Some(other) => {
                return Err(DomainError::internal(format!(
                    "GitHub device-flow error: {other}"
                )))
            }
            None => {
                return Err(DomainError::internal(
                    "token response had neither access_token nor error",
                ))
            }
        }
    }
}
