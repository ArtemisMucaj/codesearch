//! Dream scheduling for `codesearch serve`.
//!
//! [`DreamService`] wraps one shared [`MemoryDreamUseCase`] (its internal lock
//! is what serializes scheduled and manually-triggered cycles) together with
//! the resolved [`MemoryConfig`], and drives two cadences from a single loop:
//!
//! - every [`SWEEP_INTERVAL_SECS`], a **harvest sweep** imports finished
//!   sessions (idle past the configured window, never imported), so memories
//!   land promptly instead of waiting for the next full dream;
//! - whenever the persisted last-run timestamp says a full cycle is due
//!   (default every 4 h), a **dream cycle** consolidates the store.
//!
//! Scheduling state lives in the memory database (`memory_dream_runs`), so a
//! restarted server continues the cadence instead of dreaming immediately on
//! every boot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};

use crate::application::use_cases::memory_support::unix_now;
use crate::application::{MemoryDreamUseCase, MemoryRepository};
use crate::connector::adapter::{CodesearchConfig, MemoryConfig};
use crate::connector::api::Container;
use crate::domain::{DomainError, DreamRun};

/// Seconds between scheduler ticks (harvest sweep + dream-due check).
const SWEEP_INTERVAL_SECS: u64 = 15 * 60;

/// Shared dream state for serve mode: the scheduler loop and the management
/// API's status/trigger endpoints both go through this.
pub struct DreamService {
    use_case: Arc<MemoryDreamUseCase>,
    memory_repo: Arc<dyn MemoryRepository>,
    /// The scheduling config, behind a lock so a management-API write applies
    /// live: the scheduler reads a fresh snapshot each tick, so a changed
    /// interval / idle window / toggle takes effect on the next sweep without a
    /// server restart. Guarded by a plain `RwLock` (never held across `.await`).
    config: RwLock<MemoryConfig>,
    /// Data dir where `config.json` lives, so config writes can be persisted.
    data_dir: String,
    /// Whether a cycle or sweep is currently in flight (for status reporting;
    /// mutual exclusion itself lives inside the use case).
    running: AtomicBool,
}

impl DreamService {
    /// Build the service from the serve container, using the container's
    /// configured LLM target for all dream model calls and the `memory`
    /// section of `config.json` for scheduling.
    pub fn build(container: &Container) -> Result<Arc<Self>> {
        let config = CodesearchConfig::load(container.data_dir())
            .context("failed to load config.json for the dream scheduler")?
            .memory
            .unwrap_or_default();
        let chat_client = crate::connector::api::controller::build_chat_client(
            container.llm_target(),
            container.data_dir(),
        )
        .context("failed to build the dream scheduler's chat client")?;
        Ok(Arc::new(Self {
            use_case: Arc::new(
                container
                    .memory_dream_use_case(chat_client)
                    .context("failed to build the dream use case")?,
            ),
            memory_repo: container
                .memory_repository()
                .context("failed to open the memory repository for the dream scheduler")?,
            config: RwLock::new(config),
            data_dir: container.data_dir().to_string(),
            running: AtomicBool::new(false),
        }))
    }

    /// A snapshot of the current scheduling config. Cloned so callers never hold
    /// the lock (and never hold a guard across `.await`). A poisoned lock is
    /// logged (not silently swallowed) before falling back to the default.
    pub fn config(&self) -> MemoryConfig {
        self.config.read().map(|c| c.clone()).unwrap_or_else(|e| {
            tracing::warn!("dream scheduler config lock poisoned, using default: {e}");
            MemoryConfig::default()
        })
    }

    /// Apply new dream settings: persist them into `config.json`'s `memory`
    /// section (preserving every other section) and swap the in-memory config so
    /// the scheduler picks them up on its next tick. Returns the merged config.
    ///
    /// Async because the persistence step does blocking filesystem I/O
    /// (`load` + `save`), which is pushed off the runtime via `spawn_blocking`
    /// so it never stalls the async request thread.
    pub async fn update_config(
        &self,
        patch: MemoryConfigPatch,
    ) -> Result<MemoryConfig, DomainError> {
        // Reject nonsensical values up front (durations must be positive), so a
        // `0` is a clear 400 rather than a silently-ignored write — the accessors
        // treat `0` as "use the default", which would mislead the caller.
        patch.validate()?;

        // Merge onto the current in-memory config so an omitted field is left
        // unchanged rather than reset to its default.
        let mut merged = self.config();
        patch.apply(&mut merged);

        // Persist off the async thread: load the whole doc so other sections
        // (openai/copilot) survive the write, replace the memory section, save.
        let data_dir = self.data_dir.clone();
        let to_write = merged.clone();
        tokio::task::spawn_blocking(move || -> Result<(), DomainError> {
            let mut doc = CodesearchConfig::load(&data_dir)?;
            doc.memory = Some(to_write);
            doc.save(&data_dir)
        })
        .await
        .map_err(|e| DomainError::internal(format!("config write task panicked: {e}")))??;

        // Swap the live config so the scheduler reads the new values next tick.
        match self.config.write() {
            Ok(mut guard) => *guard = merged.clone(),
            Err(e) => tracing::warn!("failed to swap live dream config (lock poisoned): {e}"),
        }
        Ok(merged)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn last_run(&self) -> Option<DreamRun> {
        match self.memory_repo.last_dream_run().await {
            Ok(run) => run,
            Err(e) => {
                tracing::warn!("failed to read last dream run for status: {e}");
                None
            }
        }
    }

    fn idle_secs(&self) -> i64 {
        (self.config().session_idle_minutes() * 60) as i64
    }

    /// Start a dream cycle in the background. Returns `false` (without
    /// spawning) when one is already in flight.
    pub fn trigger(self: &Arc<Self>) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let _reset = RunningGuard(&service.running);
            service.run_cycle().await;
        });
        true
    }

    async fn run_cycle(&self) {
        match self
            .use_case
            .execute(self.idle_secs(), self.config().auto_import())
            .await
        {
            Ok(report) => tracing::info!(
                "dream cycle finished ({} sessions imported, {} ops applied, {} skipped)",
                report.sessions_imported,
                report.applied.len(),
                report.skipped.len()
            ),
            Err(e) => tracing::warn!("dream cycle failed: {e}"),
        }
    }

    /// Run the scheduler until the process exits.
    ///
    /// The loop always runs so a config change made at runtime (via
    /// `update_config`) takes effect: each tick reads a fresh config snapshot,
    /// so enabling dreaming/auto-import later starts it without a restart. When
    /// both are off, ticks are cheap no-ops.
    pub async fn run_scheduler(self: Arc<Self>) {
        let cfg = self.config();
        tracing::info!(
            "dream scheduler: sweep every {} min, dream every {} h, auto-import {}",
            SWEEP_INTERVAL_SECS / 60,
            cfg.dream_interval_hours(),
            if cfg.auto_import() { "on" } else { "off" },
        );
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first `tick()` completes immediately, so a freshly started server
        // harvests (and dreams, when due) right away.
        loop {
            ticker.tick().await;
            self.tick().await;
        }
    }

    /// One scheduler tick: run a full dream when due, else a harvest sweep.
    async fn tick(&self) {
        let cfg = self.config();
        if cfg.dream_enabled() && self.dream_due().await {
            if self.running.swap(true, Ordering::SeqCst) {
                return; // a manual trigger is in flight; try again next tick
            }
            let _reset = RunningGuard(&self.running);
            self.run_cycle().await;
            return;
        }
        if !cfg.auto_import() {
            return;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let _reset = RunningGuard(&self.running);
        match self.use_case.harvest(self.idle_secs()).await {
            Ok(report) if report.sessions_imported > 0 => tracing::info!(
                "dream sweep: imported {} finished session(s)",
                report.sessions_imported
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!("dream sweep failed: {e}"),
        }
    }

    /// A full cycle is due when none was ever recorded or the last one
    /// finished more than the configured interval ago.
    async fn dream_due(&self) -> bool {
        let interval_secs = (self.config().dream_interval_hours() * 3_600) as i64;
        match self.memory_repo.last_dream_run().await {
            Ok(Some(last)) => unix_now() - last.finished_at >= interval_secs,
            Ok(None) => true,
            Err(e) => {
                tracing::warn!("dream scheduler could not read last run: {e}");
                false
            }
        }
    }
}

/// A partial update to the dream scheduling config. Every field is optional so
/// a client can change one setting without resending the rest; an omitted field
/// leaves the current value untouched. A `0` duration is rejected by
/// [`validate`](Self::validate) — the accessors treat `0` as "use the default",
/// so accepting it would silently ignore the client's value.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct MemoryConfigPatch {
    pub dream_enabled: Option<bool>,
    pub dream_interval_hours: Option<u64>,
    pub session_idle_minutes: Option<u64>,
    pub auto_import: Option<bool>,
}

impl MemoryConfigPatch {
    /// Reject values the scheduler cannot honor. Durations must be positive:
    /// `0` would be treated as "use the default" by the accessors, so accepting
    /// it would silently ignore the client's intent — return a clear error
    /// instead (surfaced as a 400 by the handler).
    fn validate(&self) -> Result<(), DomainError> {
        if self.dream_interval_hours == Some(0) {
            return Err(DomainError::invalid_input(
                "dream_interval_hours must be at least 1",
            ));
        }
        if self.session_idle_minutes == Some(0) {
            return Err(DomainError::invalid_input(
                "session_idle_minutes must be at least 1",
            ));
        }
        Ok(())
    }

    /// Merge this patch onto `config`, overwriting only the fields it sets.
    fn apply(&self, config: &mut MemoryConfig) {
        if let Some(v) = self.dream_enabled {
            config.dream_enabled = Some(v);
        }
        if let Some(v) = self.dream_interval_hours {
            config.dream_interval_hours = Some(v);
        }
        if let Some(v) = self.session_idle_minutes {
            config.session_idle_minutes = Some(v);
        }
        if let Some(v) = self.auto_import {
            config.auto_import = Some(v);
        }
    }
}

/// Resets the shared `running` flag when dropped, so a panicking cycle or
/// sweep can never leave the scheduler wedged with `running = true`.
struct RunningGuard<'a>(&'a AtomicBool);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A patch overwrites only the fields it sets; omitted fields keep their
    /// existing values (so a client can change one setting in isolation).
    #[test]
    fn patch_applies_only_set_fields() {
        let mut config = MemoryConfig {
            dream_enabled: Some(true),
            dream_interval_hours: Some(4),
            session_idle_minutes: Some(60),
            auto_import: Some(true),
        };
        let patch = MemoryConfigPatch {
            auto_import: Some(false),
            dream_interval_hours: Some(8),
            ..Default::default()
        };
        patch.apply(&mut config);
        assert_eq!(config.auto_import, Some(false)); // changed
        assert_eq!(config.dream_interval_hours, Some(8)); // changed
        assert_eq!(config.dream_enabled, Some(true)); // untouched
        assert_eq!(config.session_idle_minutes, Some(60)); // untouched
    }

    /// An empty patch is a no-op — nothing is disturbed.
    #[test]
    fn empty_patch_changes_nothing() {
        let mut config = MemoryConfig {
            dream_enabled: Some(false),
            ..Default::default()
        };
        MemoryConfigPatch::default().apply(&mut config);
        assert_eq!(config.dream_enabled, Some(false));
        assert_eq!(config.dream_interval_hours, None);
    }

    /// The patch deserializes from a partial JSON body (missing keys → None).
    #[test]
    fn patch_deserializes_partial_body() {
        let patch: MemoryConfigPatch = serde_json::from_str(r#"{"auto_import": false}"#).unwrap();
        assert_eq!(patch.auto_import, Some(false));
        assert_eq!(patch.dream_enabled, None);
        assert_eq!(patch.dream_interval_hours, None);
    }

    /// Durations of `0` are rejected (they'd otherwise be silently treated as
    /// "use the default"); a positive or omitted duration validates.
    #[test]
    fn validate_rejects_zero_durations() {
        assert!(MemoryConfigPatch {
            dream_interval_hours: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(MemoryConfigPatch {
            session_idle_minutes: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err());
        // Positive durations and an all-toggles patch validate fine.
        assert!(MemoryConfigPatch {
            dream_interval_hours: Some(1),
            session_idle_minutes: Some(1),
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(MemoryConfigPatch {
            auto_import: Some(false),
            ..Default::default()
        }
        .validate()
        .is_ok());
        // The rejection is an InvalidInput (→ 400), not an internal error.
        let err = MemoryConfigPatch {
            dream_interval_hours: Some(0),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.is_invalid_input());
    }
}
