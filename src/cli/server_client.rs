//! Route write commands through a running `serve` process.
//!
//! DuckDB is single-writer per file. When `codesearch serve` holds the database
//! open, a one-shot CLI write command (`create`, `index`, `delete`) can't take
//! the write lock and would fail. This module detects such a server via the
//! run-info file it writes (see `connector::adapter::management::runinfo`),
//! confirms it's alive with a `/health` probe, and forwards the command to the
//! management API. Read commands don't come here — they open the DB read-only,
//! which DuckDB allows concurrently with the writer.
//!
//! Policy (chosen deliberately): if a server *owns* this data-dir but the API
//! call fails, we return an error telling the user to retry or pass `--local`.
//! We never silently fall back to opening the DB, because the server still holds
//! the lock and that fallback would just reproduce the confusing lock error.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::read_runinfo;

/// How the CLI resolves where (if anywhere) to route a write command.
pub enum RouteTarget {
    /// A live server owns this data-dir; forward to its management base URL.
    Server(String),
    /// No server detected — open the DB directly, as before.
    Local,
}

/// Short timeout for the liveness probe. A running server on loopback answers
/// `/health` in well under this; if it doesn't, treat it as not usable.
const HEALTH_TIMEOUT: Duration = Duration::from_millis(500);

/// Decide how a write command should run.
///
/// - `force_local` (from `--local`) short-circuits to [`RouteTarget::Local`].
/// - `explicit_server` (from `--server <URL>`) forces routing to that URL
///   without reading run-info (still liveness-probed).
/// - Otherwise: read run-info for `data_dir`; if present and its `/health`
///   answers, route to it; if absent, run locally; if present but dead (stale
///   file), run locally (the DB is free, so direct access will succeed).
pub async fn resolve_route(
    data_dir: &Path,
    force_local: bool,
    explicit_server: Option<&str>,
) -> Result<RouteTarget> {
    if force_local {
        return Ok(RouteTarget::Local);
    }

    if let Some(url) = explicit_server {
        let base = url.trim_end_matches('/').to_string();
        if probe_health(&base).await {
            return Ok(RouteTarget::Server(base));
        }
        bail!("no codesearch server reachable at {base} (from --server)");
    }

    let Some(info) = read_runinfo(data_dir) else {
        return Ok(RouteTarget::Local);
    };
    let base = info.mgmt_base_url();
    if probe_health(&base).await {
        // Warn (don't block) on a version mismatch: the CLI may send a request
        // shape an older server doesn't understand.
        let cli_version = env!("CARGO_PKG_VERSION");
        if info.version != cli_version {
            tracing::warn!(
                "codesearch server at {base} is version {}, CLI is {cli_version}",
                info.version
            );
        }
        Ok(RouteTarget::Server(base))
    } else {
        // Stale run-info (server gone). The DB lock is free, so run locally.
        tracing::info!("run-info present but server not responding; running locally");
        Ok(RouteTarget::Local)
    }
}

/// GET `{base}/health`; true iff it answers 2xx within the timeout.
async fn probe_health(base: &str) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(HEALTH_TIMEOUT).build() else {
        return false;
    };
    match client.get(format!("{base}/health")).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// A client bound to one server's management base URL.
pub struct ServerClient {
    base: String,
    client: reqwest::Client,
}

impl ServerClient {
    pub fn new(base: String) -> Self {
        Self {
            base,
            client: reqwest::Client::new(),
        }
    }

    /// `POST /api/namespaces` — create a namespace. Mirrors `codesearch create`.
    pub async fn create_namespace(
        &self,
        name: &str,
        embedding_target: &str,
        embedding_model: Option<&str>,
        embedding_dimensions: usize,
        no_embeddings: bool,
    ) -> Result<String> {
        let body = serde_json::json!({
            "name": name,
            "embedding_target": embedding_target,
            "embedding_model": embedding_model,
            "embedding_dimensions": embedding_dimensions,
            "no_embeddings": no_embeddings,
        });
        let resp = self
            .client
            .post(format!("{}/api/namespaces", self.base))
            .json(&body)
            .send()
            .await
            .context("failed to reach the running codesearch server")?;
        let value = read_json_or_error(resp, "create namespace").await?;
        Ok(format!(
            "Created namespace '{}' via the running server.",
            value
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
        ))
    }

    /// `DELETE /api/repositories/{id}` — delete a repository by id or path.
    pub async fn delete_repository(&self, id_or_path: &str) -> Result<String> {
        let resp = self
            .client
            .delete(format!(
                "{}/api/repositories/{}",
                self.base,
                urlencode(id_or_path)
            ))
            .send()
            .await
            .context("failed to reach the running codesearch server")?;
        read_json_or_error(resp, "delete repository").await?;
        Ok(format!("Deleted '{id_or_path}' via the running server."))
    }

    /// `POST /api/stream/index` — index a repository, consuming the SSE stream
    /// and surfacing the final `done`/`error` event as the command result.
    pub async fn index_repository(
        &self,
        path: &str,
        name: Option<&str>,
        force: bool,
    ) -> Result<String> {
        let body = serde_json::json!({ "path": path, "name": name, "force": force });
        let resp = self
            .client
            .post(format!("{}/api/stream/index", self.base))
            .json(&body)
            .send()
            .await
            .context("failed to reach the running codesearch server")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("server returned {status} for index: {text}");
        }
        consume_index_sse(resp).await
    }
}

/// Read a JSON response, mapping a non-2xx status to a useful error that
/// includes the server's `{"error": …}` message when present.
async fn read_json_or_error(resp: reqwest::Response, op: &str) -> Result<serde_json::Value> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return serde_json::from_str(&text)
            .with_context(|| format!("{op}: could not parse server response"));
    }
    let msg = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or(text);
    Err(anyhow!("{op} failed on the server ({status}): {msg}"))
}

/// Consume the index SSE stream, returning the final human-readable line built
/// from the terminal `done` event, or an error from an `error` event.
async fn consume_index_sse(resp: reqwest::Response) -> Result<String> {
    use futures_util::StreamExt;

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut current_event: Option<String> = None;
    let mut last_error: Option<String> = None;
    let mut done: Option<serde_json::Value> = None;

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("index stream interrupted")?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // SSE frames are separated by a blank line; process complete lines.
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf.drain(..=nl);

            if let Some(name) = line.strip_prefix("event:") {
                current_event = Some(name.trim().to_string());
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                let payload: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
                match current_event.as_deref() {
                    Some("done") => done = Some(payload),
                    Some("error") => {
                        last_error = Some(
                            payload
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("indexing failed")
                                .to_string(),
                        )
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(err) = last_error {
        bail!("{err}");
    }
    let done = done.ok_or_else(|| anyhow!("index stream ended without a result"))?;
    let name = done.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let files = done.get("file_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let chunks = done
        .get("chunk_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Ok(format!(
        "Indexed '{name}' via the running server: {files} files, {chunks} chunks."
    ))
}

/// Minimal path-segment encoding for the repository id/path in a URL. Percent-
/// encodes the characters that would otherwise break the path segment.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_slashes_and_spaces() {
        assert_eq!(urlencode("a/b c"), "a%2Fb%20c");
        assert_eq!(urlencode("plain-id_1.2~"), "plain-id_1.2~");
    }

    #[tokio::test]
    async fn resolve_route_force_local_skips_detection() {
        let dir = tempfile::tempdir().unwrap();
        let route = resolve_route(dir.path(), true, None).await.unwrap();
        assert!(matches!(route, RouteTarget::Local));
    }

    #[tokio::test]
    async fn resolve_route_no_runinfo_is_local() {
        let dir = tempfile::tempdir().unwrap();
        let route = resolve_route(dir.path(), false, None).await.unwrap();
        assert!(matches!(route, RouteTarget::Local));
    }
}
