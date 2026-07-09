//! Import a finished session transcript into the memory store.
//!
//! Orchestrates the OpenViking "session commit" flow: idempotence check,
//! memory extraction, and recording of the imported-session marker.

use std::sync::Arc;

use tracing::info;

use crate::application::interfaces::MemoryRepository;
use crate::application::use_cases::memory_extraction::{ExtractionReport, MemoryExtractionUseCase};
use crate::domain::{DomainError, ImportedSession, SessionTranscript};

/// Minimum number of non-empty messages a transcript must contain for
/// extraction to be worthwhile.
const MIN_MESSAGES: usize = 2;

/// Outcome of an import request.
pub enum ImportOutcome {
    /// Extraction ran; the report describes what was written.
    Imported {
        session: ImportedSession,
        report: ExtractionReport,
    },
    /// The session was already imported and `force` was not set.
    AlreadyImported { session: ImportedSession },
}

pub struct ImportSessionUseCase {
    memory_repo: Arc<dyn MemoryRepository>,
    extraction: MemoryExtractionUseCase,
}

impl ImportSessionUseCase {
    pub fn new(
        memory_repo: Arc<dyn MemoryRepository>,
        extraction: MemoryExtractionUseCase,
    ) -> Self {
        Self {
            memory_repo,
            extraction,
        }
    }

    /// Import `transcript`, running memory extraction over it.
    ///
    /// Imports are idempotent per transcript ID: a session that has already
    /// been imported is skipped unless `force` is set.
    pub async fn execute(
        &self,
        transcript: &SessionTranscript,
        force: bool,
    ) -> Result<ImportOutcome, DomainError> {
        let non_empty = transcript
            .messages
            .iter()
            .filter(|m| !m.content.trim().is_empty())
            .count();
        if non_empty < MIN_MESSAGES {
            return Err(DomainError::invalid_input(format!(
                "transcript '{}' has only {} non-empty messages (minimum {})",
                transcript.id, non_empty, MIN_MESSAGES
            )));
        }

        if !force {
            if let Some(session) = self.memory_repo.find_session(&transcript.id).await? {
                return Ok(ImportOutcome::AlreadyImported { session });
            }
        }

        let report = self.extraction.execute(transcript).await?;
        info!(
            "session '{}': {} operations applied, {} skipped",
            transcript.id,
            report.applied.len(),
            report.skipped.len()
        );

        let session = ImportedSession {
            id: transcript.id.clone(),
            source: transcript.source.clone(),
            imported_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            message_count: transcript.messages.len(),
            items_written: report.items_written(),
        };
        self.memory_repo.record_session(&session).await?;

        Ok(ImportOutcome::Imported { session, report })
    }
}
