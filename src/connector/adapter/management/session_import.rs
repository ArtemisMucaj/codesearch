//! Session discovery + background import for `codesearch serve`.
//!
//! [`SessionImportService`] is the serve-mode analogue of the interactive
//! import picker (`src/tui/import_picker.rs`): it discovers finished assistant
//! sessions, materializes a transcript on demand, and imports a chosen session
//! **in the background** so the HTTP request returns immediately and the import
//! keeps running even if the client navigates away.
//!
//! It is intentionally shaped like [`super::DreamService`]:
//! - one shared instance lives in [`super::AppState`],
//! - imports run under `tokio::spawn` and report progress into a status map,
//! - the map is keyed by a session's stable identity `(source, id)` so status
//!   survives re-discovery (the list re-sorts newest-first each time).
//!
//! The status map mirrors the picker's `ImportStatus` state machine
//! (`queued → importing → done | failed`, plus `already_imported` for sessions
//! already present in the store when discovery ran), so a native client can
//! render the exact same per-row markers the TUI does.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::application::{ImportOutcome, SessionDiscovery};
use crate::connector::adapter::LocalSessionDiscovery;
use crate::connector::api::Container;
use crate::domain::{DiscoveredSession, SessionLocator};

/// Stable identity of a discovered session: `(source, id)`. Used as the status
/// map key so a session's import status follows it across re-discovery.
type SessionKey = (String, String);

/// Import lifecycle of one session, mirroring the picker's `ImportStatus`.
///
/// Serialized in `snake_case` (`already_imported`, …) as the `status` field of
/// each entry in `GET /api/sessions/import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    /// Already present in the memory store when discovery ran.
    AlreadyImported,
    /// Accepted by `POST /api/sessions/import`, worker not yet started.
    Queued,
    /// Extraction in progress.
    Importing,
    /// Extraction finished (freshly imported or re-imported).
    Done,
    /// Extraction failed; carries a short reason.
    Failed,
}

/// A status-map entry: the current lifecycle state plus, on terminal states, a
/// one-line summary (Done) or error (Failed) for the client to surface.
#[derive(Debug, Clone, Serialize)]
pub struct StatusEntry {
    pub source: String,
    pub id: String,
    pub status: ImportStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Shared session-import state for serve mode. Discovery is stateless; the
/// status map is the only mutable state and is guarded by an async mutex (held
/// only for the brief map updates, never across an import).
pub struct SessionImportService {
    container: Arc<Container>,
    discovery: Arc<LocalSessionDiscovery>,
    status: Arc<Mutex<HashMap<SessionKey, StatusEntry>>>,
}

impl SessionImportService {
    /// Build the service from the serve container. Cheap — no LLM client is
    /// constructed until an import actually runs.
    pub fn build(container: Arc<Container>) -> Arc<Self> {
        let discovery = Arc::new(LocalSessionDiscovery::new(Some(
            container.metadata_db_path(),
        )));
        Arc::new(Self {
            container,
            discovery,
            status: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Discover all finished sessions (newest first). Seeds the status map with
    /// `already_imported` for any session already in the store, so the very
    /// first `discover` call — before any `import` — already carries the ✓
    /// markers the TUI shows on open.
    pub async fn discover(&self) -> Result<Vec<DiscoveredSession>> {
        let sessions = self.discovery.discover().await?;
        // Cross-reference the memory store's imported-session records so the
        // client can render ✓ without a second round-trip.
        let repo = self.container.memory_repository()?;
        let imported: std::collections::HashSet<String> = repo
            .list_sessions()
            .await?
            .into_iter()
            .map(|s| s.id)
            .collect();

        let mut map = self.status.lock().await;
        for s in &sessions {
            if imported.contains(&s.id) {
                let key = session_key(s);
                // Don't clobber an in-flight/finished import status.
                map.entry(key).or_insert_with(|| StatusEntry {
                    source: s.source.as_str().to_string(),
                    id: s.id.clone(),
                    status: ImportStatus::AlreadyImported,
                    detail: None,
                });
            }
        }
        Ok(sessions)
    }

    /// Materialize the transcript for the session identified by `(source, id)`.
    /// Re-discovers to resolve the opaque [`SessionLocator`] rather than
    /// trusting a client-supplied path.
    pub async fn transcript(
        &self,
        source: &str,
        id: &str,
    ) -> Result<crate::domain::SessionTranscript> {
        let session = self.find(source, id).await?;
        Ok(self.discovery.load_transcript(&session).await?)
    }

    /// Queue a background import of the session identified by `(source, id)`.
    ///
    /// Returns immediately after setting the status to `queued` and spawning the
    /// worker; the import (transcript load → memory extraction → summarization)
    /// runs on a detached task, so it survives the HTTP request completing and
    /// the client navigating away. Re-importing a done/already-imported session
    /// is allowed (extraction is forced); a session already `queued`/`importing`
    /// is a no-op so a double click can't double-run.
    pub async fn import(self: &Arc<Self>, source: &str, id: &str, force: bool) -> Result<()> {
        let session = self.find(source, id).await?;
        let key = session_key(&session);

        {
            let mut map = self.status.lock().await;
            if matches!(
                map.get(&key).map(|e| &e.status),
                Some(ImportStatus::Queued | ImportStatus::Importing)
            ) {
                // Already in flight — don't double-queue.
                return Ok(());
            }
            map.insert(key.clone(), entry(&session, ImportStatus::Queued, None));
        }

        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.run_import(session, force).await;
        });
        Ok(())
    }

    /// Every tracked session's import status, for `GET /api/sessions/import`.
    pub async fn statuses(&self) -> Vec<StatusEntry> {
        self.status.lock().await.values().cloned().collect()
    }

    /// Run one import to completion, updating the status map at each transition.
    /// Errors are recorded as `failed` rather than propagated (this runs
    /// detached, so there is no caller to return them to).
    async fn run_import(&self, session: DiscoveredSession, force: bool) {
        let key = session_key(&session);
        self.set(&key, entry(&session, ImportStatus::Importing, None))
            .await;

        match self.do_import(&session, force).await {
            Ok(summary) => {
                self.set(&key, entry(&session, ImportStatus::Done, Some(summary)))
                    .await;
            }
            Err(e) => {
                tracing::warn!("session import '{}' failed: {e:#}", session.id);
                self.set(
                    &key,
                    entry(&session, ImportStatus::Failed, Some(format!("{e:#}"))),
                )
                .await;
            }
        }
    }

    /// The import itself: build a chat client, load the transcript, run the
    /// import use case, and render a one-line outcome summary.
    async fn do_import(&self, session: &DiscoveredSession, force: bool) -> Result<String> {
        let chat_client = crate::connector::api::controller::build_chat_client(
            self.container.llm_target(),
            self.container.data_dir(),
        )
        .context("failed to build the import chat client")?;
        let use_case = self
            .container
            .memory_import_use_case(chat_client)
            .context("failed to build the import use case")?;

        let transcript = self.discovery.load_transcript(session).await?;
        let outcome = use_case.execute(&transcript, force).await?;
        Ok(match outcome {
            ImportOutcome::Imported { report, .. } => {
                let written = report.items_written();
                format!(
                    "{} memory item{} written",
                    written,
                    if written == 1 { "" } else { "s" }
                )
            }
            ImportOutcome::AlreadyImported { .. } => "already imported".to_string(),
        })
    }

    /// Re-discover and resolve one session by its `(source, id)` identity. A
    /// missing session is a `NotFound` (→ 404 at the API), not an internal error.
    async fn find(&self, source: &str, id: &str) -> Result<DiscoveredSession> {
        let sessions = self.discovery.discover().await?;
        sessions
            .into_iter()
            .find(|s| s.source.as_str() == source && s.id == id)
            .ok_or_else(|| {
                crate::domain::DomainError::not_found(format!(
                    "no discoverable session '{id}' from source '{source}'"
                ))
                .into()
            })
    }

    async fn set(&self, key: &SessionKey, value: StatusEntry) {
        self.status.lock().await.insert(key.clone(), value);
    }
}

/// Stable identity for a discovered session (mirrors the picker's `session_key`).
fn session_key(s: &DiscoveredSession) -> SessionKey {
    (s.source.as_str().to_string(), s.id.clone())
}

/// Build a status-map entry for a session.
fn entry(s: &DiscoveredSession, status: ImportStatus, detail: Option<String>) -> StatusEntry {
    StatusEntry {
        source: s.source.as_str().to_string(),
        id: s.id.clone(),
        status,
        detail,
    }
}

/// Serialize a [`DiscoveredSession`] into the JSON DTO the API returns.
///
/// `DiscoveredSession` is not `Serialize` (its [`SessionLocator`] is an opaque
/// on-disk address we deliberately never expose to clients — imports are
/// requested by `(source, id)` and re-resolved server-side). This projects only
/// the display fields the picker shows.
pub fn session_to_json(s: &DiscoveredSession) -> serde_json::Value {
    // Kept in sync with the picker's list columns: source, updated_at,
    // approx_tokens, title, plus cwd/message_count/preview for detail.
    let locator = match &s.locator {
        SessionLocator::File(_) => "file",
        SessionLocator::Sqlite { .. } => "sqlite",
    };
    serde_json::json!({
        "source": s.source.as_str(),
        "id": s.id,
        "title": s.display_title(),
        "cwd": s.cwd,
        "updated_at": s.updated_at,
        "message_count": s.message_count,
        "approx_tokens": s.approx_tokens,
        "tail_preview": s.tail_preview,
        "locator_kind": locator,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SessionLocator, SessionSource};

    fn session(source: SessionSource, id: &str) -> DiscoveredSession {
        DiscoveredSession {
            source,
            id: id.to_string(),
            title: format!("session {id}"),
            cwd: Some("/tmp/project".to_string()),
            updated_at: 1_700_000_000,
            message_count: 12,
            tail_preview: "…outcome".to_string(),
            approx_tokens: 3_400,
            locator: SessionLocator::File(format!("/logs/{id}.jsonl")),
        }
    }

    /// The DTO carries exactly the display fields the native client decodes, and
    /// never leaks the opaque on-disk locator (only its coarse kind).
    #[test]
    fn session_to_json_projects_display_fields_only() {
        let v = session_to_json(&session(SessionSource::Claude, "abc"));
        assert_eq!(v["source"], "claude");
        assert_eq!(v["id"], "abc");
        assert_eq!(v["title"], "session abc");
        assert_eq!(v["cwd"], "/tmp/project");
        assert_eq!(v["updated_at"], 1_700_000_000);
        assert_eq!(v["message_count"], 12);
        assert_eq!(v["approx_tokens"], 3_400);
        assert_eq!(v["locator_kind"], "file");
        // The raw locator path must never appear in the payload.
        assert!(!v.to_string().contains("/logs/abc.jsonl"));
    }

    /// A `(source, id)` pair keys status the same way for every source, so the
    /// same session id under two sources stays distinct.
    #[test]
    fn session_key_is_source_scoped() {
        let a = session_key(&session(SessionSource::Claude, "same"));
        let b = session_key(&session(SessionSource::OpenCode, "same"));
        assert_ne!(a, b);
        assert_eq!(a, ("claude".to_string(), "same".to_string()));
    }

    /// The status enum serializes to the snake_case strings the client decodes.
    #[test]
    fn import_status_serializes_snake_case() {
        let cases = [
            (ImportStatus::AlreadyImported, "\"already_imported\""),
            (ImportStatus::Queued, "\"queued\""),
            (ImportStatus::Importing, "\"importing\""),
            (ImportStatus::Done, "\"done\""),
            (ImportStatus::Failed, "\"failed\""),
        ];
        for (status, expected) in cases {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
        }
    }
}
