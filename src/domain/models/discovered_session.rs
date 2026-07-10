use serde::{Deserialize, Serialize};

/// Which assistant produced a discovered session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    Claude,
    OpenCode,
    Zed,
}

impl SessionSource {
    /// Short label shown in the picker (`claude`, `opencode`, `zed`).
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionSource::Claude => "claude",
            SessionSource::OpenCode => "opencode",
            SessionSource::Zed => "zed",
        }
    }
}

impl std::fmt::Display for SessionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How to locate a discovered session's full transcript so it can be
/// materialized on demand (only for sessions the user actually imports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLocator {
    /// A Claude Code transcript file on disk.
    File(String),
    /// A row in a SQLite database, addressed by DB path + session id.
    Sqlite { db_path: String, session_id: String },
}

/// A session found by the discovery layer, shown in the import picker before
/// any expensive parsing/decompression of its body.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub source: SessionSource,
    /// Stable session identifier (used for idempotent imports).
    pub id: String,
    /// Friendly, human-readable name (summary/title, else a fallback).
    pub title: String,
    /// Working directory / project the session ran in, when known.
    pub cwd: Option<String>,
    /// Unix seconds of the last activity (used for "N ago" and sorting).
    pub updated_at: i64,
    /// Number of conversational messages, when cheaply known.
    pub message_count: usize,
    /// A short preview taken from the END of the session (the outcome), so the
    /// picker shows where the conversation landed rather than where it began.
    pub tail_preview: String,
    /// Where to read the full transcript from when importing.
    pub locator: SessionLocator,
}

impl DiscoveredSession {
    /// A display title that is never empty.
    pub fn display_title(&self) -> &str {
        if self.title.trim().is_empty() {
            "(untitled session)"
        } else {
            self.title.trim()
        }
    }
}
