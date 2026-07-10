//! Discovery of OpenCode sessions from `~/.local/share/opencode/opencode.db`.
//!
//! OpenCode stores each session's metadata in a `session` row (with a `title`)
//! and its conversation as `message` rows (role) whose text lives in `part`
//! rows (`{"type":"text","text":…}`). A transcript is the ordered join of the
//! two by `time_created`.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::domain::{
    DiscoveredSession, DomainError, SessionLocator, SessionMessage, SessionSource,
    SessionTranscript,
};

use super::{home_dir, tail_preview, truncate_chars};

const MAX_TITLE_CHARS: usize = 80;
const PREVIEW_MESSAGES: usize = 6;

/// Open the OpenCode database read-only, or `Ok(None)` when it is absent.
fn open_db() -> Result<Option<Connection>, DomainError> {
    let path = home_dir()?.join(".local/share/opencode/opencode.db");
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| DomainError::storage(format!("cannot open opencode.db: {e}")))?;
    Ok(Some(conn))
}

pub fn discover() -> Result<Vec<DiscoveredSession>, DomainError> {
    let Some(conn) = open_db()? else {
        return Ok(Vec::new());
    };
    let db_path = home_dir()?
        .join(".local/share/opencode/opencode.db")
        .to_string_lossy()
        .to_string();

    // `time_updated` is milliseconds since the epoch. Skip archived sessions.
    let mut stmt = conn
        .prepare(
            "SELECT id, title, directory, time_updated \
             FROM session WHERE time_archived IS NULL ORDER BY time_updated DESC",
        )
        .map_err(|e| DomainError::storage(format!("opencode query prepare failed: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1).unwrap_or_default(),
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3).unwrap_or(0),
            ))
        })
        .map_err(|e| DomainError::storage(format!("opencode query failed: {e}")))?;

    let mut sessions = Vec::new();
    for row in rows {
        let (id, title, directory, time_updated_ms) =
            row.map_err(|e| DomainError::storage(format!("opencode row read failed: {e}")))?;

        let texts = message_texts(&conn, &id)?;
        if texts.is_empty() {
            continue;
        }
        let tail: Vec<String> = texts
            .iter()
            .rev()
            .take(PREVIEW_MESSAGES)
            .rev()
            .cloned()
            .collect();

        sessions.push(DiscoveredSession {
            source: SessionSource::OpenCode,
            title: truncate_chars(title.trim(), MAX_TITLE_CHARS),
            cwd: directory,
            updated_at: time_updated_ms / 1000,
            message_count: texts.len(),
            tail_preview: tail_preview(&tail),
            locator: SessionLocator::Sqlite {
                db_path: db_path.clone(),
                session_id: id.clone(),
            },
            id,
        });
    }

    Ok(sessions)
}

/// Load the full transcript for one OpenCode session.
pub fn load_transcript(db_path: &str, session_id: &str) -> Result<SessionTranscript, DomainError> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| DomainError::storage(format!("cannot open opencode.db: {e}")))?;

    let messages = ordered_messages(&conn, session_id)?;
    if messages.is_empty() {
        return Err(DomainError::invalid_input(format!(
            "opencode session '{session_id}' has no messages"
        )));
    }

    Ok(SessionTranscript {
        id: session_id.to_string(),
        source: format!("opencode:{session_id}"),
        messages,
    })
}

/// The plain text of each message in a session, ordered oldest→newest, used
/// for previews and counting (tool activity elided).
fn message_texts(conn: &Connection, session_id: &str) -> Result<Vec<String>, DomainError> {
    Ok(ordered_messages(conn, session_id)?
        .into_iter()
        .map(|m| m.content)
        .filter(|c| !c.trim().is_empty())
        .collect())
}

/// Build the ordered `SessionMessage` list: one per `message` row, its text the
/// concatenation of that message's `text` parts, with tool calls summarized.
fn ordered_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SessionMessage>, DomainError> {
    // Roles per message, ordered.
    let mut msg_stmt = conn
        .prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created ASC")
        .map_err(|e| DomainError::storage(format!("opencode message prepare failed: {e}")))?;
    let msg_rows = msg_stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| DomainError::storage(format!("opencode message query failed: {e}")))?;

    let mut messages = Vec::new();
    for row in msg_rows {
        let (msg_id, data) =
            row.map_err(|e| DomainError::storage(format!("opencode message read failed: {e}")))?;
        let role = serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|v| v.get("role").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "assistant".to_string());

        let text = message_part_text(conn, &msg_id)?;
        if text.trim().is_empty() {
            continue;
        }
        messages.push(SessionMessage {
            role,
            content: text,
            timestamp: None,
        });
    }
    Ok(messages)
}

/// Concatenate the renderable text of a message's parts.
fn message_part_text(conn: &Connection, message_id: &str) -> Result<String, DomainError> {
    let mut stmt = conn
        .prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created ASC")
        .map_err(|e| DomainError::storage(format!("opencode part prepare failed: {e}")))?;
    let rows = stmt
        .query_map([message_id], |row| row.get::<_, String>(0))
        .map_err(|e| DomainError::storage(format!("opencode part query failed: {e}")))?;

    let mut parts = Vec::new();
    for row in rows {
        let data = row.map_err(|e| DomainError::storage(format!("opencode part read: {e}")))?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = value.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        parts.push(t.trim().to_string());
                    }
                }
            }
            Some("tool") => {
                let name = value
                    .get("tool")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("name").and_then(Value::as_str))
                    .unwrap_or("tool");
                // The tool's arguments live in `state.input`; render them
                // compactly so the transcript shows *what* the tool was asked
                // to do, matching the Claude parser's `ToolCall:` format.
                let input = value
                    .get("state")
                    .and_then(|s| s.get("input"))
                    .map(render_tool_input)
                    .filter(|s| !s.is_empty());
                match input {
                    Some(args) => parts.push(format!("ToolCall: name={name}; input={args}")),
                    None => parts.push(format!("ToolCall: name={name}")),
                }
            }
            _ => {}
        }
    }
    Ok(parts.join("\n"))
}

/// Maximum characters of a rendered tool input (matches the Claude parser).
const MAX_TOOL_INPUT_CHARS: usize = 200;

/// Compact single-line rendering of a tool input object (`k=v, k=v`), truncated.
fn render_tool_input(input: &Value) -> String {
    let rendered = match input {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                format!("{k}={v}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    };
    let compact: String = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > MAX_TOOL_INPUT_CHARS {
        let kept: String = compact.chars().take(MAX_TOOL_INPUT_CHARS).collect();
        format!("{kept}…")
    } else {
        compact
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_tool_input_args() {
        let input = serde_json::json!({"command": "git status", "timeout": 30000});
        let rendered = render_tool_input(&input);
        assert!(rendered.contains("command=git status"));
        assert!(rendered.contains("timeout=30000"));
    }

    #[test]
    fn truncates_long_tool_input() {
        let input = serde_json::json!({"command": "x".repeat(500)});
        assert!(render_tool_input(&input).chars().count() <= MAX_TOOL_INPUT_CHARS + 1);
    }
}
