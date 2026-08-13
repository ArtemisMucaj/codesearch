//! LLM-generated display names for Leiden communities, cached by stable id.
//!
//! Cluster/community *detection* produces only a stable, content-addressed `id`.
//! This use case turns each community into a nice human-readable **display name**
//! via an LLM, lazily and with a persistent cache:
//!
//! 1. For a batch of communities, look up cached names by id
//!    ([`AnalysisRepository::get_community_names`]).
//! 2. For every cache miss, ask the [`ChatClient`] for a short label, feeding it
//!    a sample of member symbols/files and their dominant directories — no
//!    source reads, so the prompt stays cheap.
//! 3. Persist the freshly generated names ([`AnalysisRepository::save_community_names`])
//!    so subsequent renders — and future runs whose membership is unchanged —
//!    are free.
//!
//! Because names are keyed on the stable id (a pure function of membership), the
//! cache survives re-index: an unchanged community keeps its name, a changed one
//! gets a new id and is re-named on next view.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{stream, StreamExt};
use tokio::time::timeout;
use tracing::{debug, warn};

use super::cluster_detection::ancestor_dir_frequencies;
use crate::application::{AnalysisRepository, ChatClient};
use crate::domain::{Cluster, DomainError, SymbolCommunity};

/// Max concurrent LLM naming calls. The chat clients have no rate-limit backoff,
/// so this is kept conservative — enough to hide per-call latency without
/// hammering the provider.
const NAMING_CONCURRENCY: usize = 6;

/// Deadline for the serial probe call that decides whether the endpoint is
/// usable at all.
///
/// Generous enough for a cold local model (loading weights on the first request
/// can take tens of seconds) but bounded, so a server that accepts the
/// connection and then never responds degrades to ids instead of hanging the
/// naming run. Only the probe is bounded — once it answers, the endpoint has
/// proven itself and the remaining calls use the client's own timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// System prompt: the model returns a single short label, nothing else. Kept
/// terse so small local models behave; the JSON schema on the request enforces
/// the shape.
const SYSTEM_PROMPT: &str = "You name software modules. Given a group of related \
files or code symbols from one repository, reply with a concise, human-readable \
name (2–5 words, Title Case) that captures what the group is about. Prefer domain \
concepts over generic words. Do not include the repository name, punctuation, or \
any explanation.";

/// How many member names to show the model per community — enough to convey the
/// theme without bloating the prompt.
const MEMBERS_IN_PROMPT: usize = 25;

/// Anything that can be named: exposes the stable id, heuristic name, and
/// members. Level-agnostic so file clusters and symbol communities share the
/// same prompt/caching path.
trait Nameable {
    fn id(&self) -> &str;
    fn members(&self) -> &[String];
    fn set_display_name(&mut self, name: String);
}

impl Nameable for Cluster {
    fn id(&self) -> &str {
        &self.id
    }
    fn members(&self) -> &[String] {
        &self.members
    }
    fn set_display_name(&mut self, name: String) {
        self.display_name = Some(name);
    }
}

impl Nameable for SymbolCommunity {
    fn id(&self) -> &str {
        &self.id
    }
    fn members(&self) -> &[String] {
        &self.members
    }
    fn set_display_name(&mut self, name: String) {
        self.display_name = Some(name);
    }
}

/// JSON schema for the structured naming response.
fn name_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"],
        "additionalProperties": false
    })
}

/// Ids currently being named, shared by every caller that names communities.
///
/// Naming is slow (one LLM call per community, [`NAMING_CONCURRENCY`] at a time)
/// and its results only reach the cache when the whole batch finishes. A client
/// polling for names — or simply switching between the graph and cluster views —
/// therefore issues several requests inside that window, and without a claim
/// every one of them re-names the same ids, multiplying LLM traffic on what is
/// usually a single local server.
///
/// Held behind an `Arc` and handed to the use case rather than owned by it: the
/// container builds a fresh [`CommunityNamingUseCase`] per call, so instance
/// state would dedupe nothing. A process-wide `static` would work too, but this
/// keeps the registry injectable — tests construct their own and cannot leak
/// claims into each other.
#[derive(Debug, Default)]
pub struct NamingRegistry {
    in_flight: Mutex<HashSet<String>>,
}

impl NamingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the ids in `ids` that nobody else is naming.
    ///
    /// Returns the claim, or `None` when every id is already in flight — the
    /// caller then has nothing to do. A partially-overlapping batch still claims
    /// its new ids rather than being refused wholesale.
    fn claim(self: &Arc<Self>, ids: impl Iterator<Item = String>) -> Option<NamingClaim> {
        let mut in_flight = self.in_flight.lock().ok()?;
        let claimed: Vec<String> = ids.filter(|id| in_flight.insert(id.clone())).collect();
        if claimed.is_empty() {
            return None;
        }
        Some(NamingClaim {
            registry: Arc::clone(self),
            ids: claimed,
        })
    }

    fn release(&self, ids: &[String]) {
        if let Ok(mut in_flight) = self.in_flight.lock() {
            for id in ids {
                in_flight.remove(id);
            }
        }
    }
}

/// Ids claimed for naming, released on drop.
///
/// A guard rather than a bare remove: naming can end at any `await` (a cancelled
/// runtime, a panic inside the use case), and a claim leaked by an early exit
/// would suppress naming for those ids until the process restarts.
struct NamingClaim {
    registry: Arc<NamingRegistry>,
    ids: Vec<String>,
}

impl Drop for NamingClaim {
    fn drop(&mut self) {
        self.registry.release(&self.ids);
    }
}

/// Use case: fill in `display_name` on communities, generating missing names via
/// the LLM and caching them by stable id.
pub struct CommunityNamingUseCase {
    storage: Arc<dyn AnalysisRepository>,
    registry: Arc<NamingRegistry>,
}

impl CommunityNamingUseCase {
    pub fn new(storage: Arc<dyn AnalysisRepository>, registry: Arc<NamingRegistry>) -> Self {
        Self { storage, registry }
    }

    /// Enrich file clusters with LLM display names in place.
    pub async fn name_clusters(&self, clusters: &mut [Cluster], chat: &dyn ChatClient) {
        self.name_all(clusters, chat).await;
    }

    /// Enrich symbol communities with LLM display names in place.
    pub async fn name_symbol_communities(
        &self,
        communities: &mut [SymbolCommunity],
        chat: &dyn ChatClient,
    ) {
        self.name_all(communities, chat).await;
    }

    /// Shared implementation over anything [`Nameable`].
    ///
    /// Naming is best-effort: a cache read failure or an LLM error leaves the
    /// affected community's `display_name` as `None` (the caller then shows the
    /// id) rather than failing the whole command.
    async fn name_all<T: Nameable>(&self, items: &mut [T], chat: &dyn ChatClient) {
        if items.is_empty() {
            return;
        }

        let ids: Vec<String> = items.iter().map(|c| c.id().to_string()).collect();
        let cached = self
            .storage
            .get_community_names(&ids)
            .await
            .unwrap_or_else(|e| {
                warn!("community-name cache read failed, regenerating: {e}");
                HashMap::new()
            });

        // Apply cache hits in place; collect the misses to name via the LLM.
        // Each miss is (item index, members) so results can be written back by
        // index after the concurrent generation.
        let mut misses: Vec<(usize, Vec<String>)> = Vec::new();
        for (idx, item) in items.iter_mut().enumerate() {
            match cached.get(item.id()) {
                Some(name) => item.set_display_name(name.clone()),
                None => misses.push((idx, item.members().to_vec())),
            }
        }
        if misses.is_empty() {
            return;
        }

        // Claim the misses, after the cache read so a warm cache never blocks and
        // never waits on the lock. Dropping the ids another task already holds
        // leaves those items on their id — that task is generating the name and
        // will cache it, so the next request picks it up.
        let miss_ids = misses.iter().map(|(idx, _)| items[*idx].id().to_string());
        let Some(_claim) = self.registry.claim(miss_ids) else {
            debug!("community naming already in flight for these ids; skipping");
            return;
        };
        let claimed: HashSet<&str> = _claim.ids.iter().map(String::as_str).collect();
        let misses: Vec<(usize, Vec<String>)> = misses
            .into_iter()
            .filter(|(idx, _)| claimed.contains(items[*idx].id()))
            .collect();
        if misses.is_empty() {
            return;
        }

        // Probe with the first miss serially. Naming runs by default, so when no
        // endpoint is reachable this one call fails fast and we skip the rest —
        // rather than firing a timeout per community and leaving everything on the
        // id fallback the slow way.
        //
        // Bounded, because "unreachable" is not the only failure: a local server
        // can accept the connection and then never answer (a model that reports
        // itself loaded but is wedged emits no bytes at all). Without a deadline
        // the probe inherits the client's, so the whole naming run hangs on one
        // dead model instead of degrading to ids.
        let (first_idx, first_members) = &misses[0];
        let first = match timeout(PROBE_TIMEOUT, generate_name(first_members, chat)).await {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    "LLM naming timed out after {}s on the first community; showing ids. \
                     The configured model may be loaded but not serving completions.",
                    PROBE_TIMEOUT.as_secs()
                );
                return;
            }
        };
        if let Err(e) = &first {
            // Warn, not debug: this is the difference between "the LLM had
            // nothing to say" and "naming is silently disabled", and the server
            // runs at info level, so a debug line here is invisible exactly when
            // someone is asking why every community shows an id.
            warn!("LLM naming unavailable ({e}); showing ids");
            return;
        }

        // Persist the probe's name before the batch runs. Naming a large
        // repository takes one LLM call per community at NAMING_CONCURRENCY at a
        // time, so deferring every write to the end leaves the cache empty for
        // the whole run — the window in which concurrent requests find nothing
        // and regenerate. Flushing as names land shrinks that window to a single
        // call and makes partial progress survive a crash mid-run.
        if let Ok(name) = first {
            items[*first_idx].set_display_name(name.clone());
            self.persist(&[(items[*first_idx].id().to_string(), name)])
                .await;
        }

        // Endpoint is up — generate the remaining misses concurrently, bounded by
        // NAMING_CONCURRENCY (the clients have no backoff, so the bound protects
        // the provider).
        let rest: Vec<(usize, Vec<String>)> = misses.into_iter().skip(1).collect();
        let mut generated = stream::iter(rest)
            .map(|(idx, members)| async move { (idx, generate_name(&members, chat).await) })
            .buffer_unordered(NAMING_CONCURRENCY);

        // Flushed in batches rather than per name: one write per community would
        // multiply round-trips to the store for no benefit, since a concurrent
        // reader only needs the window bounded, not eliminated.
        let mut pending: Vec<(String, String)> = Vec::with_capacity(NAMING_CONCURRENCY);
        while let Some((idx, result)) = generated.next().await {
            match result {
                Ok(name) => {
                    pending.push((items[idx].id().to_string(), name.clone()));
                    items[idx].set_display_name(name);
                }
                Err(e) => debug!("skipping LLM name for {}: {e}", items[idx].id()),
            }
            if pending.len() >= NAMING_CONCURRENCY {
                self.persist(&pending).await;
                pending.clear();
            }
        }
        if !pending.is_empty() {
            self.persist(&pending).await;
        }
    }

    /// Write freshly generated names to the cache, logging a failure without
    /// propagating it — naming is best-effort and a dropped write only costs a
    /// regeneration.
    async fn persist(&self, names: &[(String, String)]) {
        if names.is_empty() {
            return;
        }
        if let Err(e) = self.storage.save_community_names(names).await {
            // Louder than a read miss: a dropped write means these names are
            // regenerated (and re-billed to the LLM) on every future run.
            warn!("failed to persist community names, they will be regenerated: {e}");
        }
    }
}

/// Ask the LLM for one community's display name.
async fn generate_name(members: &[String], chat: &dyn ChatClient) -> Result<String, DomainError> {
    let prompt = build_prompt(members);
    let raw = chat
        .complete_json(SYSTEM_PROMPT, &prompt, "community_name", &name_schema())
        .await?;
    let name = parse_name(&raw).unwrap_or_default();
    let cleaned = name.trim().trim_matches('"').trim();
    if cleaned.is_empty() {
        return Err(DomainError::internal(
            "LLM returned an empty community name",
        ));
    }
    Ok(cleaned.to_string())
}

/// Extract the `name` field from the model's (schema-constrained) JSON, tolerating
/// a bare string for providers that ignore the schema.
fn parse_name(raw: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
            return Some(name.to_string());
        }
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
    }
    // Not JSON at all — take the first non-empty line as the name.
    raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Build the user prompt: a sample of members and the dominant directories that
/// the members share.
fn build_prompt(members: &[String]) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format!("This group has {} members. ", members.len()));
    prompt.push_str("A sample of member names:\n");
    for m in members.iter().take(MEMBERS_IN_PROMPT) {
        prompt.push_str(&format!("  - {m}\n"));
    }
    if members.len() > MEMBERS_IN_PROMPT {
        prompt.push_str(&format!(
            "  … and {} more\n",
            members.len() - MEMBERS_IN_PROMPT
        ));
    }

    let dirs = top_directories(members);
    if !dirs.is_empty() {
        prompt.push_str("\nCommon locations:\n");
        for (dir, count) in dirs {
            prompt.push_str(&format!("  - {dir} ({count} members)\n"));
        }
    }

    prompt.push_str("\nReply with the module name only.");
    prompt
}

/// The three directories most members share, as a cheap structural hint for the
/// model. Reuses [`ancestor_dir_frequencies`] so the directory walk matches the
/// heuristic-naming path.
fn top_directories(members: &[String]) -> Vec<(String, usize)> {
    let mut dirs: Vec<(String, usize)> = ancestor_dir_frequencies(members).into_iter().collect();
    // Most-covered first, then deeper, then lexicographic — deterministic.
    dirs.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.0.matches('/').count().cmp(&a.0.matches('/').count()))
            .then(a.0.cmp(&b.0))
    });
    dirs.truncate(3);
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The duplicate-work case: a second request arriving while the first is
    /// still generating must not re-name the same communities.
    #[test]
    fn a_second_claim_on_the_same_ids_is_refused() {
        let registry = Arc::new(NamingRegistry::new());
        let held = registry
            .claim(ids(&["a", "b"]).into_iter())
            .expect("first claim should succeed");

        assert!(
            registry.claim(ids(&["a", "b"]).into_iter()).is_none(),
            "ids already in flight must not be claimed twice"
        );

        drop(held);
    }

    /// The failure a `Drop` guard prevents: naming ending early (a panic, a
    /// cancelled runtime) would otherwise leak its claim and suppress those ids
    /// until the process restarts.
    #[test]
    fn dropping_the_claim_releases_the_ids() {
        let registry = Arc::new(NamingRegistry::new());
        drop(
            registry
                .claim(ids(&["a"]).into_iter())
                .expect("first claim should succeed"),
        );

        assert!(
            registry.claim(ids(&["a"]).into_iter()).is_some(),
            "ids must be reclaimable once the previous run finished"
        );
    }

    /// A request overlapping an in-flight one still names what is new, rather
    /// than being refused wholesale and leaving fresh communities unnamed.
    #[test]
    fn only_the_unclaimed_subset_is_taken() {
        let registry = Arc::new(NamingRegistry::new());
        let held = registry
            .claim(ids(&["a", "b"]).into_iter())
            .expect("first claim");

        let second = registry
            .claim(ids(&["a", "c"]).into_iter())
            .expect("the new id should still be claimable");
        assert_eq!(second.ids, ids(&["c"]));

        drop(held);
    }

    /// Two registries are independent, which is what makes the type injectable:
    /// a test (or a second server instance) cannot be affected by claims made
    /// elsewhere. This is the property the previous process-global `static`
    /// could not offer.
    #[test]
    fn registries_are_independent() {
        let one = Arc::new(NamingRegistry::new());
        let two = Arc::new(NamingRegistry::new());

        let _held = one.claim(ids(&["a"]).into_iter()).expect("claim on one");

        assert!(
            two.claim(ids(&["a"]).into_iter()).is_some(),
            "a claim in one registry must not block another"
        );
    }

    #[test]
    fn test_parse_name_from_json() {
        assert_eq!(
            parse_name(r#"{"name": "Camera Event Models"}"#),
            Some("Camera Event Models".to_string())
        );
    }

    #[test]
    fn test_parse_name_from_bare_string() {
        assert_eq!(
            parse_name(r#""Heating Control""#),
            Some("Heating Control".to_string())
        );
    }

    #[test]
    fn test_parse_name_from_plain_text() {
        assert_eq!(
            parse_name("Payment Processing\n"),
            Some("Payment Processing".to_string())
        );
    }

    #[test]
    fn test_top_directories_ranks_shared() {
        let members = vec![
            "src/models/events/a.php".to_string(),
            "src/models/events/b.php".to_string(),
            "src/models/devices/c.php".to_string(),
        ];
        let dirs = top_directories(&members);
        // src/models covers all 3 and should rank first.
        assert_eq!(dirs[0], ("src/models".to_string(), 3));
    }

    #[test]
    fn test_build_prompt_includes_members_and_dirs() {
        let members = vec![
            "src/models/events/Camera.php".to_string(),
            "src/models/events/Doorbell.php".to_string(),
        ];
        let prompt = build_prompt(&members);
        assert!(prompt.contains("Camera.php"));
        assert!(prompt.contains("src/models/events"));
    }
}
