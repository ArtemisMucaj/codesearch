//! Channel endpoint — cross-service links (Kafka topics, HTTP routes, MQTT/AMQP
//! topics, gRPC methods) between the repositories in the namespace.
//!
//! - `GET /api/channels` — matched producer→consumer channel links

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::application::{ChannelLinkOptions, ChannelLinkReport};
use crate::domain::Protocol;

use super::super::error::{ApiError, ApiResult};
use super::super::server::AppState;

/// Query params for `GET /api/channels`.
///
/// `repository` and `exclude_channel` are **comma-separated** strings, not
/// repeated keys: axum's default `Query` (serde_urlencoded) can't deserialize a
/// `Vec` from a query string, so `?repository=a&repository=b` fails to bind and
/// the filter is silently dropped. Comma lists (`?repository=a,b`) parse
/// reliably as a single string that the handler splits.
#[derive(Debug, Deserialize)]
pub struct ChannelsParams {
    /// Restrict to specific repositories (name or UUID), comma-separated. Omit
    /// to scope to the current namespace's repositories (matching the CLI).
    #[serde(default)]
    pub repository: Option<String>,
    /// Filter by protocol: kafka, http, mqtt, amqp, or grpc.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Drop edges whose confidence is below this threshold (0.0–1.0).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Exclude channels matching these globs (e.g. `/health*`), comma-separated.
    #[serde(default)]
    pub exclude_channel: Option<String>,
    /// Include endpoints from test files (excluded by default).
    #[serde(default)]
    pub include_tests: bool,
}

/// Split a comma-separated query value into trimmed, non-empty items.
fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `GET /api/channels` — compute cross-service channel links. Returns the
/// structured [`ChannelLinkReport`].
pub async fn channels(
    State(state): State<AppState>,
    Query(params): Query<ChannelsParams>,
) -> ApiResult<Json<ChannelLinkReport>> {
    let protocol = match params.protocol {
        Some(p) => Some(Protocol::parse(&p).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown protocol '{p}' (expected kafka, http, mqtt, amqp, or grpc)"
            ))
        })?),
        None => None,
    };

    let all_repos = state.container.list_use_case().execute().await?;

    // Mirror the CLI: `channel_endpoints` is a single global table (not
    // namespace-scoped), so an unfiltered query would leak endpoints from every
    // namespace. When no repository is named, scope to this namespace's repos.
    let repository_ids: Option<Vec<String>> = match params.repository.as_deref().map(csv) {
        Some(keys) if !keys.is_empty() => {
            let mut ids = Vec::new();
            for key in keys {
                ids.push(super::resolve_repo(&key, &all_repos)?.id().to_string());
            }
            Some(ids)
        }
        _ => {
            let namespace = state.container.namespace();
            Some(
                all_repos
                    .iter()
                    .filter(|r| r.namespace() == Some(namespace))
                    .map(|r| r.id().to_string())
                    .collect(),
            )
        }
    };

    let options = ChannelLinkOptions {
        protocol,
        min_confidence: params.min_confidence,
        exclude_channels: params
            .exclude_channel
            .as_deref()
            .map(csv)
            .unwrap_or_default(),
        include_tests: params.include_tests,
    };
    let report = state
        .container
        .channel_link_use_case()
        .link(repository_ids.as_deref(), &options)
        .await?;

    Ok(Json(report))
}
