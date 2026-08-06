//! Server run-info file: how a one-shot CLI invocation discovers that a
//! `serve` process is already running against the same data directory.
//!
//! DuckDB is single-writer per file. When `codesearch serve` holds the database
//! open, any CLI subcommand that needs a *write* lock (`create`, `index`,
//! `delete`) can't open it and fails. The fix is to route those commands
//! through the running server's management API instead — but first the CLI has
//! to find it. On startup `serve` writes this small JSON file into the data
//! directory recording the management port and pid; the CLI reads it, probes
//! `/health`, and if the server is live routes the write through it. The file
//! is removed on graceful shutdown; a stale file (server killed) is detected by
//! the health probe failing, so the CLI never trusts it blindly.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename written into the data directory (next to `codesearch.duckdb`).
const RUNINFO_FILE: &str = "serve.json";

/// What a running `serve` advertises about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeRunInfo {
    /// Management API port (`--mgmt-port`). This is the REST surface the CLI
    /// routes write commands through.
    pub mgmt_port: u16,
    /// MCP HTTP port (`--mcp-port`). Recorded for completeness / diagnostics.
    pub mcp_port: u16,
    /// PID of the `serve` process, for diagnostics and stale-file reasoning.
    pub pid: u32,
    /// Crate version of the running server, so a mismatched CLI can warn.
    pub version: String,
}

impl ServeRunInfo {
    /// The base URL of the management API on loopback.
    pub fn mgmt_base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.mgmt_port)
    }
}

/// Absolute path to the run-info file for a given data directory.
pub fn runinfo_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RUNINFO_FILE)
}

/// Write the run-info file. Called once, when `serve` has bound its ports.
///
/// Best-effort: a failure to write only means the CLI can't auto-detect this
/// server and will fall back to opening the DB directly, so we log and carry on
/// rather than failing the server.
pub fn write(data_dir: &Path, info: &ServeRunInfo) {
    let path = runinfo_path(data_dir);
    match serde_json::to_vec_pretty(info) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, bytes) {
                tracing::warn!("could not write serve run-info at {}: {e}", path.display());
            } else {
                tracing::info!("wrote serve run-info at {}", path.display());
            }
        }
        Err(e) => tracing::warn!("could not serialize serve run-info: {e}"),
    }
}

/// Remove the run-info file. Called on graceful shutdown. Best-effort — a
/// leftover file is harmless because readers verify liveness via `/health`.
pub fn remove(data_dir: &Path) {
    let path = runinfo_path(data_dir);
    if let Err(e) = std::fs::remove_file(&path) {
        // Not existing is fine (e.g. write failed at startup).
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("could not remove serve run-info at {}: {e}", path.display());
        }
    }
}

/// Read the run-info file, if present and parseable. `None` means "no server
/// advertised here" — the caller should proceed with direct DB access.
pub fn read(data_dir: &Path) -> Option<ServeRunInfo> {
    let path = runinfo_path(data_dir);
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(info) => Some(info),
        Err(e) => {
            tracing::warn!(
                "ignoring unparseable serve run-info at {}: {e}",
                path.display()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ServeRunInfo {
        ServeRunInfo {
            mgmt_port: 8676,
            mcp_port: 8677,
            pid: 4242,
            version: "9.9.9".to_string(),
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &sample());
        let got = read(dir.path()).expect("run-info should be readable");
        assert_eq!(got.mgmt_port, 8676);
        assert_eq!(got.mcp_port, 8677);
        assert_eq!(got.pid, 4242);
        assert_eq!(got.version, "9.9.9");
    }

    #[test]
    fn read_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_none());
    }

    #[test]
    fn read_garbage_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(runinfo_path(dir.path()), b"not json").unwrap();
        assert!(read(dir.path()).is_none());
    }

    #[test]
    fn remove_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &sample());
        assert!(read(dir.path()).is_some());
        remove(dir.path());
        assert!(read(dir.path()).is_none());
    }

    #[test]
    fn remove_absent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        remove(dir.path()); // must not panic
    }

    #[test]
    fn mgmt_base_url_is_loopback() {
        assert_eq!(sample().mgmt_base_url(), "http://127.0.0.1:8676");
    }
}
