use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::{DomainError, ExecutionFeature, FeatureNode};

// ──────────────────────────────────────────────────────────────────────────────
// Criticality scoring constants (weights must sum to 1.0)
// ──────────────────────────────────────────────────────────────────────────────

const WEIGHT_FILE_SPREAD: f32 = 0.35;
const WEIGHT_EXTERNAL_CALLS: f32 = 0.25;
const WEIGHT_TEST_COVERAGE_GAP: f32 = 0.25;
const WEIGHT_DEPTH: f32 = 0.15;

/// Soft reference depth for depth-score normalisation. A path reaching this
/// depth scores 1.0 on the depth signal; deeper paths are clamped to 1.0.
const DEPTH_REFERENCE: f32 = 20.0;

/// Score assigned when no test symbol directly calls the entry point.
const TEST_COVERAGE_GAP_SCORE: f32 = 0.30;
/// Score assigned when a test caller IS present.
const TEST_COVERAGE_PRESENT_SCORE: f32 = 0.05;

// ──────────────────────────────────────────────────────────────────────────────
// Use case
// ──────────────────────────────────────────────────────────────────────────────

/// Use case that discovers execution features — named forward call chains
/// rooted at entry-point symbols — and scores each one for criticality.
pub struct ExecutionFeaturesUseCase {
    call_graph: Arc<CallGraphUseCase>,
}

impl ExecutionFeaturesUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>) -> Self {
        Self { call_graph }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Public API
    // ──────────────────────────────────────────────────────────────────────────

    /// Detect all entry points for `repository_id` and compute their features,
    /// returning up to `limit` results sorted by descending criticality.
    pub async fn list_features(
        &self,
        repository_id: &str,
        limit: usize,
    ) -> Result<Vec<ExecutionFeature>, DomainError> {
        let entry_points = self.find_entry_points(repository_id).await?;
        let mut features = Vec::with_capacity(entry_points.len().min(limit));

        for ep in entry_points {
            let feature = self.build_feature(&ep, repository_id).await?;
            features.push(feature);
        }

        features.sort_by(|a, b| b.criticality.partial_cmp(&a.criticality).unwrap_or(std::cmp::Ordering::Equal));
        features.truncate(limit);
        Ok(features)
    }

    /// Retrieve a single feature by entry-point symbol name (exact or substring).
    ///
    /// Returns `None` when the symbol cannot be found in the call graph or is
    /// not an entry point.
    pub async fn get_feature(
        &self,
        symbol: &str,
        repository_id: Option<&str>,
    ) -> Result<Option<ExecutionFeature>, DomainError> {
        let mut query = CallGraphQuery::new();
        if let Some(repo) = repository_id {
            query = query.with_repository(repo);
        }

        // Resolve the symbol to a fully-qualified name.
        let resolved = self
            .call_graph
            .resolve_symbols(symbol, &query, 10)
            .await?;

        if resolved.is_empty() {
            return Ok(None);
        }

        let fqn = resolved[0].clone();

        // Determine the effective repository, either from the caller-supplied
        // hint or by discovering it from the resolved symbol's call-graph edges.
        let effective_repo: String = if let Some(repo) = repository_id {
            repo.to_string()
        } else {
            let discovery_query = CallGraphQuery::new();
            // Check outgoing edges first; entry points typically have them.
            let callees = self
                .call_graph
                .find_callees(&fqn, &discovery_query)
                .await?;
            if let Some(r) = callees.first() {
                r.repository_id().to_string()
            } else {
                let callers = self
                    .call_graph
                    .find_callers(&fqn, &discovery_query)
                    .await?;
                callers
                    .first()
                    .map(|r| r.repository_id().to_string())
                    .unwrap_or_default()
            }
        };

        // Verify the resolved symbol is actually an entry point: nothing within
        // the same repository calls it.
        let repo_query = CallGraphQuery::new().with_repository(&effective_repo);
        let callers_in_repo = self.call_graph.find_callers(&fqn, &repo_query).await?;
        if !callers_in_repo.is_empty() {
            return Ok(None);
        }

        let feature = self.build_feature(&fqn, &effective_repo).await?;
        Ok(Some(feature))
    }

    /// Given a set of changed symbols, return every feature whose call chain
    /// includes at least one of them, sorted by descending criticality.
    pub async fn get_impacted_features(
        &self,
        changed_symbols: &[String],
        repository_id: Option<&str>,
    ) -> Result<Vec<ExecutionFeature>, DomainError> {
        if changed_symbols.is_empty() {
            return Ok(vec![]);
        }

        let changed_set: HashSet<&str> = changed_symbols.iter().map(String::as_str).collect();

        // Determine which repositories to scan.
        let repo_ids: Vec<String> = if let Some(repo) = repository_id {
            vec![repo.to_string()]
        } else {
            // Collect every repository that any of the changed symbols appears in.
            let mut repos: HashSet<String> = HashSet::new();
            for sym in changed_symbols {
                let query = CallGraphQuery::new();
                let callers = self.call_graph.find_callers(sym, &query).await?;
                for r in &callers {
                    repos.insert(r.repository_id().to_string());
                }
                let callees = self.call_graph.find_callees(sym, &query).await?;
                for r in &callees {
                    repos.insert(r.repository_id().to_string());
                }
            }
            repos.into_iter().collect()
        };

        let mut affected: Vec<ExecutionFeature> = Vec::new();
        for repo in &repo_ids {
            let features = self.list_features(repo, usize::MAX).await?;
            for feature in features {
                let symbols_in_path: HashSet<&str> =
                    feature.path.iter().map(|n| n.symbol.as_str()).collect();
                if changed_set.iter().any(|s| symbols_in_path.contains(*s)) {
                    affected.push(feature);
                }
            }
        }

        affected.sort_by(|a, b| b.criticality.partial_cmp(&a.criticality).unwrap_or(std::cmp::Ordering::Equal));
        Ok(affected)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Entry-point detection
    // ──────────────────────────────────────────────────────────────────────────

    /// Return the set of fully-qualified symbols in `repository_id` that are
    /// entry points: symbols that call at least one other symbol but are
    /// themselves never called within the repository.
    async fn find_entry_points(&self, repository_id: &str) -> Result<Vec<String>, DomainError> {
        let all_refs = self.call_graph.find_by_repository(repository_id).await?;

        let mut callee_symbols: HashSet<String> = HashSet::new();
        let mut caller_symbols: HashSet<String> = HashSet::new();

        for r in &all_refs {
            callee_symbols.insert(r.callee_symbol().to_string());
            if let Some(caller) = r.caller_symbol() {
                caller_symbols.insert(caller.to_string());
            }
        }

        // Entry point = calls something (has outgoing edges) AND is never called
        // (has no incoming edges within this repository).
        let mut entry_points: Vec<String> = caller_symbols
            .into_iter()
            .filter(|sym| !callee_symbols.contains(sym))
            .collect();

        entry_points.sort();
        Ok(entry_points)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Feature construction
    // ──────────────────────────────────────────────────────────────────────────

    /// Build an `ExecutionFeature` for `entry_point` in `repository_id` by
    /// running a forward BFS through the call graph and scoring the result.
    async fn build_feature(
        &self,
        entry_point: &str,
        repository_id: &str,
    ) -> Result<ExecutionFeature, DomainError> {
        let query = CallGraphQuery::new().with_repository(repository_id);

        // ── Forward BFS ────────────────────────────────────────────────────
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize, String, u32)> = VecDeque::new();
        let mut path: Vec<FeatureNode> = Vec::new();
        // Symbols that have at least one outgoing edge in this repo.
        // Non-root nodes absent from this set are BFS leaves (unresolved / external).
        let mut symbols_with_callees: HashSet<String> = HashSet::new();
        let mut total_callees_seen: usize = 0;

        visited.insert(entry_point.to_string());

        // Pre-fetch the entry-point's callees to (a) resolve its own file path
        // so the root node is not seeded with an empty string, and (b) avoid a
        // redundant call_graph lookup on the first BFS iteration.
        let initial_callees = self.call_graph.find_callees(entry_point, &query).await?;
        let entry_file_path = initial_callees
            .first()
            .map(|r| r.caller_file_path().to_string())
            .unwrap_or_default();

        path.push(FeatureNode {
            symbol: entry_point.to_string(),
            file_path: entry_file_path,
            line: 0,
            depth: 0,
            repository_id: repository_id.to_string(),
        });

        if !initial_callees.is_empty() {
            symbols_with_callees.insert(entry_point.to_string());
        }
        for reference in &initial_callees {
            total_callees_seen += 1;
            let callee = reference.callee_symbol().to_string();
            if !visited.contains(&callee) {
                visited.insert(callee.clone());
                queue.push_back((
                    callee,
                    1,
                    reference.reference_file_path().to_string(),
                    reference.reference_line(),
                ));
            }
        }

        while let Some((current, depth, file_path, line)) = queue.pop_front() {
            path.push(FeatureNode {
                symbol: current.clone(),
                file_path,
                line,
                depth,
                repository_id: repository_id.to_string(),
            });

            let callees = self.call_graph.find_callees(&current, &query).await?;
            if !callees.is_empty() {
                symbols_with_callees.insert(current.clone());
            }
            for reference in &callees {
                total_callees_seen += 1;
                let callee = reference.callee_symbol().to_string();
                if visited.contains(&callee) {
                    continue;
                }
                visited.insert(callee.clone());
                queue.push_back((
                    callee,
                    depth + 1,
                    reference.reference_file_path().to_string(),
                    reference.reference_line(),
                ));
            }
        }

        // Unresolved callees: non-root nodes with no outgoing edges in this
        // repository — a proxy for external / stdlib calls we cannot trace.
        let unresolved_callees = path
            .iter()
            .skip(1)
            .filter(|n| !symbols_with_callees.contains(&n.symbol))
            .count();

        // Avoid division by zero in the external_score calculation below.
        if total_callees_seen == 0 {
            total_callees_seen = 1;
        }

        // ── Criticality scoring ────────────────────────────────────────────
        let total_nodes = path.len().max(1);
        let distinct_files: HashSet<&str> = path.iter().map(|n| n.file_path.as_str()).collect();
        let file_count = distinct_files.len();
        let feature_depth = path.iter().map(|n| n.depth).max().unwrap_or(0);

        // Signal 1 — file spread: ratio of distinct files to total path nodes.
        let file_spread_score = (file_count as f32 / total_nodes as f32).min(1.0);

        // Signal 2 — external calls: ratio of leaf nodes (no outgoing edges in
        // this repo) to all callee references seen during BFS.
        let external_score = (unresolved_callees as f32 / total_callees_seen as f32).min(1.0);

        // Signal 3 — test coverage gap: high when nothing directly calls the
        // entry point with a test_ / it_ prefix.
        let callers_query = CallGraphQuery::new().with_repository(repository_id);
        let callers_of_entry = self
            .call_graph
            .find_callers(entry_point, &callers_query)
            .await?;
        let has_test_caller = callers_of_entry.iter().any(|r| {
            r.caller_symbol()
                .map(|s| is_test_symbol(s))
                .unwrap_or(false)
        });
        let test_gap_score = if has_test_caller {
            TEST_COVERAGE_PRESENT_SCORE
        } else {
            TEST_COVERAGE_GAP_SCORE
        };

        // Signal 4 — depth: normalised call-chain length.
        let depth_score = (feature_depth as f32 / DEPTH_REFERENCE).min(1.0);

        let criticality = (WEIGHT_FILE_SPREAD * file_spread_score
            + WEIGHT_EXTERNAL_CALLS * external_score
            + WEIGHT_TEST_COVERAGE_GAP * test_gap_score
            + WEIGHT_DEPTH * depth_score)
            .min(1.0_f32);

        let name = short_name(entry_point);
        let id = format!("{}:{}", repository_id, entry_point);

        Ok(ExecutionFeature {
            id,
            name,
            entry_point: entry_point.to_string(),
            repository_id: repository_id.to_string(),
            file_count,
            depth: feature_depth,
            path,
            criticality,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Extract the short (human-readable) name from a fully-qualified symbol.
///
/// Strips everything up to and including the last `/`, `#`, `.`, or `::`.
fn short_name(fqn: &str) -> String {
    // Handle SCIP-style `path/to/file Package#Method`.
    let base = fqn.rsplit_once('#').map(|(_, r)| r).unwrap_or(fqn);
    // Handle `::` separators (Rust, C++).
    let base = base.rsplit_once("::").map(|(_, r)| r).unwrap_or(base);
    // Handle `.` separators (Python, Java).
    let base = base.rsplit_once('.').map(|(_, r)| r).unwrap_or(base);
    // Strip trailing `()` or generic parameters.
    let base = base.split('(').next().unwrap_or(base);
    let base = base.split('<').next().unwrap_or(base);
    base.trim().to_string()
}

/// Return `true` when a symbol looks like a test function.
fn is_test_symbol(symbol: &str) -> bool {
    let sn = short_name(symbol);
    let sn_lower = sn.to_lowercase();
    sn_lower.starts_with("test_") || sn_lower.starts_with("it_")
}
