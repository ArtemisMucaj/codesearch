//! LLM-generated display names for Leiden communities, cached by stable id.
//!
//! Cluster/community *detection* produces a heuristic `name` (a directory- or
//! keyword-derived slug) and a stable, content-addressed `id`. This use case
//! turns those into nice human-readable **display names** via an LLM, lazily and
//! with a persistent cache:
//!
//! 1. For a batch of communities, look up cached names by id
//!    ([`AnalysisRepository::get_community_names`]).
//! 2. For every cache miss, ask the [`ChatClient`] for a short label, feeding it
//!    the community's heuristic name plus a sample of member symbols/files and
//!    its dominant directories — no source reads, so the prompt stays cheap.
//! 3. Persist the freshly generated names ([`AnalysisRepository::save_community_names`])
//!    so subsequent renders — and future runs whose membership is unchanged —
//!    are free.
//!
//! Because names are keyed on the stable id (a pure function of membership), the
//! cache survives re-index: an unchanged community keeps its name, a changed one
//! gets a new id and is re-named on next view.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{stream, StreamExt};
use tracing::{debug, warn};

use super::cluster_detection::ancestor_dir_frequencies;
use crate::application::{AnalysisRepository, ChatClient};
use crate::domain::{Cluster, DomainError, SymbolCommunity};

/// Max concurrent LLM naming calls. The chat clients have no rate-limit backoff,
/// so this is kept conservative — enough to hide per-call latency without
/// hammering the provider.
const NAMING_CONCURRENCY: usize = 6;

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

/// Use case: fill in `display_name` on communities, generating missing names via
/// the LLM and caching them by stable id.
pub struct CommunityNamingUseCase {
    storage: Arc<dyn AnalysisRepository>,
}

impl CommunityNamingUseCase {
    pub fn new(storage: Arc<dyn AnalysisRepository>) -> Self {
        Self { storage }
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

        // Probe with the first miss serially. Naming runs by default, so when no
        // endpoint is reachable this one call fails fast and we skip the rest —
        // rather than firing a timeout per community and leaving everything on the
        // id fallback the slow way.
        let (first_idx, first_members) = &misses[0];
        let first = generate_name(first_members, chat).await;
        if let Err(e) = &first {
            debug!("LLM naming unavailable ({e}); showing ids");
            return;
        }

        let mut fresh: Vec<(String, String)> = Vec::new();
        if let Ok(name) = first {
            fresh.push((items[*first_idx].id().to_string(), name.clone()));
            items[*first_idx].set_display_name(name);
        }

        // Endpoint is up — generate the remaining misses concurrently, bounded by
        // NAMING_CONCURRENCY (the clients have no backoff, so the bound protects
        // the provider).
        let rest: Vec<(usize, Vec<String>)> = misses.into_iter().skip(1).collect();
        let generated: Vec<(usize, Result<String, DomainError>)> = stream::iter(rest)
            .map(|(idx, members)| async move { (idx, generate_name(&members, chat).await) })
            .buffer_unordered(NAMING_CONCURRENCY)
            .collect()
            .await;

        for (idx, result) in generated {
            match result {
                Ok(name) => {
                    fresh.push((items[idx].id().to_string(), name.clone()));
                    items[idx].set_display_name(name);
                }
                Err(e) => debug!("skipping LLM name for {}: {e}", items[idx].id()),
            }
        }

        if !fresh.is_empty() {
            if let Err(e) = self.storage.save_community_names(&fresh).await {
                debug!("skipping community-name cache write: {e}");
            }
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
