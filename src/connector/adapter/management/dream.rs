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
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::application::{DreamOptions, MemoryDreamUseCase, MemoryRepository};
use crate::connector::adapter::{CodesearchConfig, MemoryConfig};
use crate::connector::api::Container;
use crate::domain::DreamRun;

/// Seconds between scheduler ticks (harvest sweep + dream-due check).
const SWEEP_INTERVAL_SECS: u64 = 15 * 60;

/// Shared dream state for serve mode: the scheduler loop and the management
/// API's status/trigger endpoints both go through this.
pub struct DreamService {
    use_case: Arc<MemoryDreamUseCase>,
    memory_repo: Arc<dyn MemoryRepository>,
    config: MemoryConfig,
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
            use_case: Arc::new(container.memory_dream_use_case(chat_client)?),
            memory_repo: container.memory_repository()?,
            config,
            running: AtomicBool::new(false),
        }))
    }

    pub fn config(&self) -> &MemoryConfig {
        &self.config
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn last_run(&self) -> Option<DreamRun> {
        self.memory_repo.last_dream_run().await.ok().flatten()
    }

    fn idle_secs(&self) -> i64 {
        (self.config.session_idle_minutes() * 60) as i64
    }

    /// Start a dream cycle in the background. Returns `false` (without
    /// spawning) when one is already in flight.
    pub fn trigger(self: &Arc<Self>, dry_run: bool, force: bool) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.run_cycle(dry_run, force).await;
            service.running.store(false, Ordering::SeqCst);
        });
        true
    }

    async fn run_cycle(&self, dry_run: bool, force: bool) {
        let options = DreamOptions {
            session_idle_secs: self.idle_secs(),
            dry_run,
            force,
        };
        match self.use_case.execute(&options).await {
            Ok(report) => tracing::info!(
                "dream cycle: {} ({} sessions imported, {} ops applied, {} skipped)",
                report.outcome,
                report.sessions_imported,
                report.applied.len(),
                report.skipped.len()
            ),
            Err(e) => tracing::warn!("dream cycle failed: {e}"),
        }
    }

    /// Run the scheduler until the process exits. Never returns unless both
    /// scheduled behaviours are disabled in config.
    pub async fn run_scheduler(self: Arc<Self>) {
        if !self.config.dream_enabled() && !self.config.auto_import() {
            tracing::info!("dream scheduler disabled by config (memory.dream_enabled=false, memory.auto_import=false)");
            return;
        }
        tracing::info!(
            "dream scheduler: sweep every {} min, dream every {} h, auto-import {}",
            SWEEP_INTERVAL_SECS / 60,
            self.config.dream_interval_hours(),
            if self.config.auto_import() {
                "on"
            } else {
                "off"
            },
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
        if self.config.dream_enabled() && self.dream_due().await {
            if self.running.swap(true, Ordering::SeqCst) {
                return; // a manual trigger is in flight; try again next tick
            }
            self.run_cycle(false, false).await;
            self.running.store(false, Ordering::SeqCst);
            return;
        }
        if !self.config.auto_import() {
            return;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        match self.use_case.harvest(self.idle_secs()).await {
            Ok(report) if report.sessions_imported > 0 => tracing::info!(
                "dream sweep: imported {} finished session(s)",
                report.sessions_imported
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!("dream sweep failed: {e}"),
        }
        self.running.store(false, Ordering::SeqCst);
    }

    /// A full cycle is due when none was ever recorded or the last one
    /// finished more than the configured interval ago.
    async fn dream_due(&self) -> bool {
        let interval_secs = (self.config.dream_interval_hours() * 3_600) as i64;
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

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
