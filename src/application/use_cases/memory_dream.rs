//! Dream — offline consolidation of the memory store.
//!
//! Per-session extraction ([`memory_extraction`](super::memory_extraction))
//! merges new information only into the handful of memories it prefetches, so
//! duplicates, contradictions, and cross-session patterns accumulate between
//! items that were never in the same extraction context. A dream cycle is the
//! global pass that cleans this up, in four phases:
//!
//! 1. **Harvest** — discover finished sessions (idle for at least an hour)
//!    that were never imported, and run them through the import pipeline.
//! 2. **Consolidate** — cluster near-duplicate items by embedding similarity,
//!    then let the model merge each cluster. Contradictions are the priority:
//!    conflicting memories are rewritten into one item carrying the boundary
//!    insight (under which conditions each side holds) instead of dropping a
//!    side.
//! 3. **Reflect** — one pass over the whole store proposing a few higher-level
//!    items: repeated experiences promoted to a skill, per-project facts
//!    generalized to global.
//! 4. **Refresh** — regenerate the whole-memory rollup and record the run.
//!
//! Guardrails keep a misbehaving model from wrecking the store: operations are
//! capped per run, consolidation may only delete items belonging to the
//! cluster it was shown, reflection may not delete at all, and total deletions
//! are bounded by a fraction of the store.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::application::interfaces::{
    ChatClient, EmbeddingService, MemoryRepository, SessionDiscovery,
};
use crate::application::use_cases::import_session::{ImportOutcome, ImportSessionUseCase};
use crate::application::use_cases::memory_dream_prompt as prompt;
use crate::application::use_cases::memory_extraction::{
    extract_json_object, normalize_name, repair_json_string_escapes,
};
use crate::application::use_cases::memory_summary::SummarizeMemoryUseCase;
use crate::application::use_cases::memory_support::{unix_now, upsert_preserving_identity};
use crate::domain::{
    cosine_similarity, DomainError, DreamRun, MemoryItem, MemoryKind, MemoryOperation,
};

/// Default idle time after which a discovered session counts as finished.
pub const DEFAULT_SESSION_IDLE_SECS: i64 = 3_600;

/// Cosine similarity above which two items are considered the same topic and
/// clustered for consolidation.
const SIMILARITY_THRESHOLD: f32 = 0.82;

/// Most clusters examined per cycle (largest first); the rest wait for the
/// next dream, keeping a cycle's LLM cost bounded.
const MAX_CLUSTERS_PER_RUN: usize = 8;

/// Most items sent to the model per cluster (most recently updated first).
const MAX_CLUSTER_ITEMS: usize = 6;

/// Most sessions imported by one harvest, so a first run over a large backlog
/// does not turn into hundreds of extraction calls. The rest are picked up by
/// subsequent cycles.
const MAX_HARVEST_SESSIONS: usize = 10;

/// Upper bound on operations applied by one dream cycle.
const MAX_DREAM_OPERATIONS: usize = 32;

/// Most items reflection may propose per cycle.
const MAX_REFLECTION_ITEMS: usize = 5;

/// Reflection is skipped below this store size — too little evidence for
/// cross-item patterns to exist.
const MIN_REFLECTION_ITEMS: usize = 4;

/// Delete budget per cycle: a fifth of the store, with a floor of one so a
/// small store can still merge a duplicate pair. A model gone wrong can
/// therefore never wipe more than a fraction of the store in one run; the
/// rest of a large cleanup waits for later cycles.
const DELETE_CAP_DIVISOR: usize = 5;

/// What one dream cycle did.
#[derive(Debug, Default)]
pub struct DreamReport {
    /// Finished, never-imported sessions found by discovery.
    pub sessions_eligible: usize,
    /// Sessions actually imported this cycle.
    pub sessions_imported: usize,
    /// Similarity clusters examined by consolidation.
    pub clusters_found: usize,
    /// Operations applied, in order.
    pub applied: Vec<MemoryOperation>,
    /// Operations rejected by a guardrail, with the reason.
    pub skipped: Vec<(MemoryOperation, String)>,
}

/// Result of a standalone harvest sweep (serve mode runs these between full
/// dream cycles so finished sessions are imported promptly).
#[derive(Debug, Default)]
pub struct HarvestReport {
    pub sessions_eligible: usize,
    pub sessions_imported: usize,
}

/// JSON shape the consolidation/reflection model must return.
#[derive(Debug, Deserialize)]
struct DreamOutput {
    #[serde(default)]
    items: Vec<RawDreamItem>,
    #[serde(default)]
    delete: Vec<RawDreamDelete>,
}

#[derive(Debug, Deserialize)]
struct RawDreamItem {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    content: String,
    /// Project scope, or `null`/absent for a global item.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawDreamDelete {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
}

pub struct MemoryDreamUseCase {
    memory_repo: Arc<dyn MemoryRepository>,
    chat_client: Arc<dyn ChatClient>,
    embedding_service: Arc<dyn EmbeddingService>,
    discovery: Arc<dyn SessionDiscovery>,
    import: ImportSessionUseCase,
    summary: SummarizeMemoryUseCase,
    /// Serializes cycles: a scheduled dream and a manual trigger must never
    /// interleave writes. `try_lock` makes the loser fail fast instead of
    /// queueing a redundant second cycle.
    running: tokio::sync::Mutex<()>,
}

impl MemoryDreamUseCase {
    pub fn new(
        memory_repo: Arc<dyn MemoryRepository>,
        chat_client: Arc<dyn ChatClient>,
        embedding_service: Arc<dyn EmbeddingService>,
        discovery: Arc<dyn SessionDiscovery>,
        import: ImportSessionUseCase,
        summary: SummarizeMemoryUseCase,
    ) -> Self {
        Self {
            memory_repo,
            chat_client,
            embedding_service,
            discovery,
            import,
            summary,
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Run one full dream cycle. `session_idle_secs` is how long a session
    /// must have been inactive to count as finished.
    #[tracing::instrument(skip_all)]
    pub async fn execute(&self, session_idle_secs: i64) -> Result<DreamReport, DomainError> {
        let _guard = self
            .running
            .try_lock()
            .map_err(|_| DomainError::invalid_input("a dream cycle is already running"))?;
        let started_at = unix_now();
        let mut report = DreamReport::default();

        // Phase 1 — harvest.
        let harvest = self.harvest_inner(session_idle_secs).await?;
        report.sessions_eligible = harvest.sessions_eligible;
        report.sessions_imported = harvest.sessions_imported;

        let items = self.memory_repo.list_items(None).await?;

        // Phase 2 — consolidate near-duplicate clusters.
        let delete_budget = (items.len() / DELETE_CAP_DIVISOR).max(1);
        let mut deletes_used = 0usize;
        let clusters = self.build_clusters(&items).await?;
        report.clusters_found = clusters.len();
        for cluster in clusters {
            let operations = match self.consolidate_cluster(&cluster).await {
                Ok(ops) => ops,
                Err(e) => {
                    warn!("dream consolidation call failed, skipping cluster: {e}");
                    continue;
                }
            };
            // Consolidation may only delete what it was shown.
            let deletable: HashSet<(MemoryKind, String)> = cluster
                .iter()
                .map(|item| (item.kind(), item.name().to_string()))
                .collect();
            self.apply(
                operations,
                Some(&deletable),
                delete_budget,
                &mut deletes_used,
                &mut report,
            )
            .await?;
        }

        // Phase 3 — reflect over the whole store (writes only, no deletes).
        // Reload first: consolidation just rewrote/deleted items, and stale
        // inputs would let reflection resurrect what was merged away.
        let items = self.memory_repo.list_items(None).await?;
        if items.len() >= MIN_REFLECTION_ITEMS {
            match self.reflect(&items).await {
                Ok(operations) => {
                    self.apply(
                        operations,
                        Some(&HashSet::new()), // nothing is deletable
                        delete_budget,
                        &mut deletes_used,
                        &mut report,
                    )
                    .await?;
                }
                Err(e) => warn!("dream reflection call failed, skipping: {e}"),
            }
        }

        // Phase 4 — refresh the rollups and record the run.
        if !report.applied.is_empty() {
            if let Err(e) = self.summary.regenerate_rollup().await {
                warn!("dream: failed to regenerate memory rollup: {e}");
            }
        }
        // Per-scope rollups check their own staleness, so this only spends
        // model calls on scopes the cycle (or anything since the last one)
        // actually touched.
        if let Err(e) = self.summary.regenerate_scope_rollups().await {
            warn!("dream: failed to regenerate scope rollups: {e}");
        }
        self.record_run(&report, started_at).await;
        info!(
            "dream cycle finished: {} imported, {} clusters, {} ops applied, {} skipped",
            report.sessions_imported,
            report.clusters_found,
            report.applied.len(),
            report.skipped.len()
        );
        Ok(report)
    }

    /// Import finished, never-imported sessions (the harvest phase alone).
    /// Serve mode calls this on a short interval between full dream cycles.
    pub async fn harvest(&self, session_idle_secs: i64) -> Result<HarvestReport, DomainError> {
        let _guard = self
            .running
            .try_lock()
            .map_err(|_| DomainError::invalid_input("a dream cycle is already running"))?;
        self.harvest_inner(session_idle_secs).await
    }

    async fn harvest_inner(&self, session_idle_secs: i64) -> Result<HarvestReport, DomainError> {
        let mut report = HarvestReport::default();
        let sessions = self.discovery.discover().await?;
        let imported: HashSet<String> = self
            .memory_repo
            .list_sessions()
            .await?
            .into_iter()
            .map(|s| s.id)
            .collect();
        let now = unix_now();

        for session in sessions {
            if session.updated_at <= 0 || now - session.updated_at < session_idle_secs {
                continue;
            }
            if imported.contains(&session.id) {
                continue;
            }
            report.sessions_eligible += 1;
            if report.sessions_imported >= MAX_HARVEST_SESSIONS {
                continue;
            }
            let transcript = match self.discovery.load_transcript(&session).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        "dream harvest: could not load session '{}': {e}",
                        session.id
                    );
                    continue;
                }
            };
            match self.import.execute(&transcript, false).await {
                Ok(ImportOutcome::Imported { session, .. }) => {
                    info!("dream harvest: imported session '{}'", session.id);
                    report.sessions_imported += 1;
                }
                Ok(ImportOutcome::AlreadyImported { .. }) => {}
                Err(e) => {
                    warn!("dream harvest: import of '{}' failed: {e}", session.id);
                }
            }
        }
        Ok(report)
    }

    /// Group items into similarity clusters (connected components over pairs
    /// whose embedding cosine similarity crosses the threshold). Items without
    /// a stored vector cannot be clustered and are left alone.
    async fn build_clusters(
        &self,
        items: &[MemoryItem],
    ) -> Result<Vec<Vec<MemoryItem>>, DomainError> {
        let vectors = self.memory_repo.list_item_vectors().await?;
        let by_id: std::collections::HashMap<&str, &MemoryItem> =
            items.iter().map(|item| (item.id(), item)).collect();
        // Keep only vectors whose item still exists, in a stable order.
        let embedded: Vec<(&MemoryItem, &Vec<f32>)> = vectors
            .iter()
            .filter_map(|(id, vector)| by_id.get(id.as_str()).map(|item| (*item, vector)))
            .collect();

        let mut parent: Vec<usize> = (0..embedded.len()).collect();
        for a in 0..embedded.len() {
            for b in (a + 1)..embedded.len() {
                if cosine_similarity(embedded[a].1, embedded[b].1) >= SIMILARITY_THRESHOLD {
                    union(&mut parent, a, b);
                }
            }
        }

        // Group indices first; only the items surviving the caps get cloned.
        let mut groups: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for idx in 0..embedded.len() {
            groups.entry(find(&mut parent, idx)).or_default().push(idx);
        }

        let mut clusters: Vec<Vec<usize>> = groups
            .into_values()
            .filter(|group| group.len() >= 2)
            .collect();
        for cluster in &mut clusters {
            // Most recently updated first; the model sees the freshest take at
            // the top and the prompt truncation drops the stalest.
            cluster.sort_by(|&a, &b| {
                embedded[b]
                    .0
                    .updated_at()
                    .cmp(&embedded[a].0.updated_at())
                    .then_with(|| embedded[a].0.name().cmp(embedded[b].0.name()))
            });
            cluster.truncate(MAX_CLUSTER_ITEMS);
        }
        // Largest (most redundant) clusters first; a deterministic tiebreak
        // keeps runs reproducible.
        clusters.sort_by(|a, b| {
            b.len()
                .cmp(&a.len())
                .then_with(|| embedded[a[0]].0.name().cmp(embedded[b[0]].0.name()))
        });
        clusters.truncate(MAX_CLUSTERS_PER_RUN);
        debug!("dream: {} consolidation clusters", clusters.len());
        Ok(clusters
            .into_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|idx| embedded[idx].0.clone())
                    .collect()
            })
            .collect())
    }

    /// One consolidation call for one cluster, with a format-recovery retry.
    async fn consolidate_cluster(
        &self,
        cluster: &[MemoryItem],
    ) -> Result<Vec<MemoryOperation>, DomainError> {
        let system = prompt::consolidation_system_prompt();
        let user = prompt::consolidation_user_prompt(cluster);
        self.complete_operations(&system, &user).await
    }

    /// One reflection call over the whole store, with a format-recovery retry.
    /// Proposed items are capped; deletes are stripped by the caller.
    async fn reflect(&self, items: &[MemoryItem]) -> Result<Vec<MemoryOperation>, DomainError> {
        let system = prompt::reflection_system_prompt(MAX_REFLECTION_ITEMS);
        let user = prompt::reflection_user_prompt(items);
        let mut operations = self.complete_operations(&system, &user).await?;
        let mut kept = 0usize;
        operations.retain(|op| match op {
            MemoryOperation::Upsert { .. } => {
                kept += 1;
                kept <= MAX_REFLECTION_ITEMS
            }
            MemoryOperation::Delete { .. } => true, // rejected later with a reason
        });
        Ok(operations)
    }

    /// Send one dream prompt and parse its operations, retrying once with a
    /// format-correction message when the output is unparseable.
    async fn complete_operations(
        &self,
        system: &str,
        user: &str,
    ) -> Result<Vec<MemoryOperation>, DomainError> {
        let schema = prompt::dream_schema();
        let response = self
            .chat_client
            .complete_json(system, user, "memory_dream", &schema)
            .await?;
        match parse_dream_operations(&response) {
            Ok(ops) => Ok(ops),
            Err(first_err) => {
                debug!("dream output unparseable, retrying once: {first_err}");
                let retry_user = format!("{user}\n\n{}", prompt::format_retry_prompt());
                let response = self
                    .chat_client
                    .complete_json(system, &retry_user, "memory_dream", &schema)
                    .await?;
                parse_dream_operations(&response).map_err(|e| {
                    DomainError::parse(format!(
                        "dream model returned unparseable output twice: {e}"
                    ))
                })
            }
        }
    }

    /// Apply validated operations under the run-level guardrails. `deletable`
    /// restricts which `(kind, name)` keys may be deleted (`Some(empty)`
    /// forbids deletion outright).
    async fn apply(
        &self,
        operations: Vec<MemoryOperation>,
        deletable: Option<&HashSet<(MemoryKind, String)>>,
        delete_budget: usize,
        deletes_used: &mut usize,
        report: &mut DreamReport,
    ) -> Result<(), DomainError> {
        // Names upserted this cycle must not be deleted by a later operation
        // of the same cycle (a model merging A+B into A sometimes also lists A
        // for deletion).
        let mut upserted: HashSet<(MemoryKind, String)> = report
            .applied
            .iter()
            .filter_map(|op| match op {
                MemoryOperation::Upsert { kind, name, .. } => Some((*kind, name.clone())),
                MemoryOperation::Delete { .. } => None,
            })
            .collect();

        for op in operations {
            if report.applied.len() >= MAX_DREAM_OPERATIONS {
                report
                    .skipped
                    .push((op, "operation limit reached".to_string()));
                continue;
            }
            match op {
                MemoryOperation::Upsert { kind, ref name, .. } => {
                    upserted.insert((kind, name.clone()));
                    self.apply_upsert(&op).await?;
                    report.applied.push(op);
                }
                MemoryOperation::Delete { kind, ref name } => {
                    let key = (kind, name.clone());
                    if upserted.contains(&key) {
                        report
                            .skipped
                            .push((op, "name was upserted this cycle".to_string()));
                        continue;
                    }
                    if let Some(allowed) = deletable {
                        if !allowed.contains(&key) {
                            report.skipped.push((
                                op,
                                "delete target was not part of the examined cluster".to_string(),
                            ));
                            continue;
                        }
                    }
                    if *deletes_used >= delete_budget {
                        report
                            .skipped
                            .push((op, "delete budget for this cycle exhausted".to_string()));
                        continue;
                    }
                    if self.memory_repo.delete_item(kind, name).await? {
                        *deletes_used += 1;
                        report.applied.push(op);
                    } else {
                        report.skipped.push((op, "item not found".to_string()));
                    }
                }
            }
        }
        Ok(())
    }

    /// Write one upsert through the shared identity-preserving path (dream
    /// keeps the existing item's source session, so no override).
    async fn apply_upsert(&self, op: &MemoryOperation) -> Result<(), DomainError> {
        let MemoryOperation::Upsert {
            kind,
            name,
            content,
            scope,
        } = op
        else {
            return Ok(());
        };
        upsert_preserving_identity(
            self.memory_repo.as_ref(),
            self.embedding_service.as_ref(),
            *kind,
            name,
            content,
            scope.clone(),
            None,
            unix_now(),
        )
        .await
    }

    /// Best-effort persistence of the run record (a bookkeeping failure must
    /// not fail a cycle whose memory writes already succeeded).
    async fn record_run(&self, report: &DreamReport, started_at: i64) {
        let run = DreamRun {
            id: uuid::Uuid::new_v4().to_string(),
            started_at,
            finished_at: unix_now(),
            sessions_imported: report.sessions_imported,
            clusters_found: report.clusters_found,
            operations_applied: report.applied.len(),
            operations_skipped: report.skipped.len(),
        };
        if let Err(e) = self.memory_repo.record_dream_run(&run).await {
            warn!("failed to record dream run: {e}");
        }
    }
}

/// Parse the model's dream JSON into validated, normalized operations,
/// tolerating prose/fences and the invalid-escape output of small models.
fn parse_dream_operations(response: &str) -> Result<Vec<MemoryOperation>, DomainError> {
    let json = extract_json_object(response)
        .ok_or_else(|| DomainError::parse("no JSON object found in dream output"))?;
    let output: DreamOutput = match serde_json::from_str(json) {
        Ok(output) => output,
        Err(strict_err) => {
            let repaired = repair_json_string_escapes(json);
            serde_json::from_str(&repaired)
                .map_err(|_| DomainError::parse(format!("invalid dream JSON: {strict_err}")))?
        }
    };

    let mut operations = Vec::new();
    for item in output.items {
        let Some(kind) = MemoryKind::parse(&item.kind) else {
            continue;
        };
        let Some(name) = normalize_name(&item.name) else {
            continue;
        };
        let content = item.content.trim();
        if content.is_empty() {
            continue;
        }
        let scope = item
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
            .map(str::to_string);
        operations.push(MemoryOperation::Upsert {
            kind,
            name,
            content: content.to_string(),
            scope,
        });
    }
    for del in output.delete {
        let Some(kind) = MemoryKind::parse(&del.kind) else {
            continue;
        };
        let Some(name) = normalize_name(&del.name) else {
            continue;
        };
        operations.push(MemoryOperation::Delete { kind, name });
    }
    Ok(operations)
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[rb] = ra;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dream_items_and_deletes() {
        let response = r#"{"items": [
            {"kind": "experience", "name": "Duckdb Locking", "content": "conflicts on concurrent writers", "scope": null},
            {"kind": "fact", "name": "sdk_version", "content": "pinned to 2.1", "scope": "svc-a"}
        ], "delete": [{"kind": "fact", "name": "old_take"}]}"#;
        let ops = parse_dream_operations(response).unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(
            ops[0],
            MemoryOperation::Upsert {
                kind: MemoryKind::Experience,
                name: "duckdb_locking".to_string(),
                content: "conflicts on concurrent writers".to_string(),
                scope: None,
            }
        );
        let MemoryOperation::Upsert { scope, .. } = &ops[1] else {
            panic!("expected upsert");
        };
        assert_eq!(scope.as_deref(), Some("svc-a"));
        assert_eq!(
            ops[2],
            MemoryOperation::Delete {
                kind: MemoryKind::Fact,
                name: "old_take".to_string(),
            }
        );
    }

    #[test]
    fn dream_parse_skips_unknown_kinds_and_empty_content() {
        let response = r#"{"items": [
            {"kind": "opinion", "name": "x", "content": "y", "scope": null},
            {"kind": "fact", "name": "ok", "content": "  ", "scope": null}
        ], "delete": [{"kind": "nope", "name": "x"}]}"#;
        let ops = parse_dream_operations(response).unwrap();
        assert!(ops.is_empty());
    }

    #[test]
    fn dream_parse_treats_string_null_scope_as_global() {
        let response = r#"{"items": [
            {"kind": "fact", "name": "n", "content": "c", "scope": "null"}
        ], "delete": []}"#;
        let ops = parse_dream_operations(response).unwrap();
        let MemoryOperation::Upsert { scope, .. } = &ops[0] else {
            panic!("expected upsert");
        };
        assert_eq!(*scope, None);
    }

    #[test]
    fn union_find_groups_transitively() {
        let mut parent: Vec<usize> = (0..4).collect();
        union(&mut parent, 0, 1);
        union(&mut parent, 1, 2);
        assert_eq!(find(&mut parent, 2), find(&mut parent, 0));
        assert_ne!(find(&mut parent, 3), find(&mut parent, 0));
    }
}
