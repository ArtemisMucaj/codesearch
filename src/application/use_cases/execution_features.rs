use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use tracing::{debug, warn};

use super::execution_features_naming::short_name;
use crate::application::{AnalysisRepository, CallGraphQuery, CallGraphUseCase};
use crate::domain::{DomainError, ExecutionFeature, FeatureNode, ReferenceKind, SymbolReference};

// ──────────────────────────────────────────────────────────────────────────────
// Criticality scoring constants (weights must sum to 1.0)
// ──────────────────────────────────────────────────────────────────────────────

/// Reachability dominates: a feature is "real" to the extent that it
/// transitively drives many symbols. Depth (call-chain length) and file spread
/// are secondary shape signals.
const WEIGHT_REACH: f32 = 0.55;
const WEIGHT_DEPTH: f32 = 0.30;
const WEIGHT_FILE_SPREAD: f32 = 0.15;

/// Soft reference depth for depth-score normalisation. A path reaching this
/// depth scores 1.0 on the depth signal; deeper paths are clamped to 1.0.
const DEPTH_REFERENCE: f32 = 12.0;

/// Soft reference reach for reach-score normalisation. A feature reaching this
/// many distinct symbols scores 1.0 on the reach signal; wider ones clamp to 1.0.
const REACH_REFERENCE: f32 = 40.0;

/// Returns `true` when a reference represents an actual execution edge (a call
/// that transfers control at run time), as opposed to a structural reference
/// such as an import, type reference, or field access.
///
/// SCIP-imported graphs are dominated by `Unknown` references (imports, type
/// occurrences, symbol mentions); traversing those as if they were calls
/// produces shallow, meaningless "features". Restricting the call graph to real
/// call edges is what makes reachability a faithful measure of a flow.
fn is_execution_edge(reference: &SymbolReference) -> bool {
    matches!(
        reference.reference_kind(),
        ReferenceKind::Call
            | ReferenceKind::MethodCall
            | ReferenceKind::Instantiation
            | ReferenceKind::MacroInvocation
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Use case
// ──────────────────────────────────────────────────────────────────────────────

/// Use case that discovers execution features — named forward call chains
/// rooted at entry-point symbols — and scores each one for criticality.
pub struct ExecutionFeaturesUseCase {
    call_graph: Arc<CallGraphUseCase>,
    /// Optional persistence for computed features. When present, the complete
    /// feature set of a repository is cached in the database and served from
    /// there until the call graph is re-indexed.
    storage: Option<Arc<dyn AnalysisRepository>>,
}

impl ExecutionFeaturesUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>) -> Self {
        Self {
            call_graph,
            storage: None,
        }
    }

    /// Attach persistent storage so computed features are cached in the
    /// database instead of being recomputed on every query.
    pub fn with_storage(mut self, storage: Arc<dyn AnalysisRepository>) -> Self {
        self.storage = Some(storage);
        self
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Public API
    // ──────────────────────────────────────────────────────────────────────────

    /// Detect all entry points for `repository_id` and compute their features,
    /// returning up to `limit` results sorted by descending criticality.
    ///
    /// The complete (untruncated) set is served from storage when available
    /// and persisted after a fresh computation, so `limit` only shapes the
    /// returned page, never what is cached.
    pub async fn list_features(
        &self,
        repository_id: &str,
        limit: usize,
    ) -> Result<Vec<ExecutionFeature>, DomainError> {
        let mut features = match self.load_stored(repository_id).await {
            Some(stored) => stored,
            None => {
                let features = self.compute_all_features(repository_id).await?;
                self.store(repository_id, &features).await;
                features
            }
        };

        features.truncate(limit);
        Ok(features)
    }

    /// Compute every entry-point feature for `repository_id`, sorted by
    /// descending criticality.
    async fn compute_all_features(
        &self,
        repository_id: &str,
    ) -> Result<Vec<ExecutionFeature>, DomainError> {
        let entry_points = self.find_entry_points(repository_id).await?;
        let mut features = Vec::with_capacity(entry_points.len());

        for ep in entry_points {
            let feature = self.build_feature(&ep, repository_id).await?;
            features.push(feature);
        }

        features.sort_by(|a, b| b.criticality.total_cmp(&a.criticality));
        Ok(features)
    }

    /// Load the stored feature set, if storage is attached and has one.
    /// Storage read failures degrade to a recompute rather than failing the
    /// query.
    async fn load_stored(&self, repository_id: &str) -> Option<Vec<ExecutionFeature>> {
        let storage = self.storage.as_ref()?;
        match storage.load_execution_features(repository_id).await {
            Ok(stored) => stored,
            Err(e) => {
                warn!("Failed to load stored execution features, recomputing: {e}");
                None
            }
        }
    }

    /// Persist a freshly computed feature set, best-effort. Failures are
    /// expected on read-only database connections and only cost the cache.
    async fn store(&self, repository_id: &str, features: &[ExecutionFeature]) {
        if let Some(storage) = &self.storage {
            if let Err(e) = storage
                .save_execution_features(repository_id, features)
                .await
            {
                debug!("Skipping execution-feature persistence: {e}");
            }
        }
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

        // Cache-first fast path: when the repository is known and its feature
        // set is cached, an exact entry-point match can be served without
        // touching the live call graph at all. Exact match is unambiguous, so
        // this never returns a different result than the graph path would;
        // substring names still fall through to `resolve_symbols` below.
        if let Some(repo) = repository_id {
            if let Some(stored) = self.load_stored(repo).await {
                if let Some(feature) = stored.into_iter().find(|f| f.entry_point == symbol) {
                    return Ok(Some(feature));
                }
            }
        }

        // Resolve the symbol to a fully-qualified name.
        let resolved = self.call_graph.resolve_symbols(symbol, &query, 10).await?;

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
            // Only real call edges count: SCIP-imported graphs are dominated by
            // structural references (imports, type occurrences), so the *first*
            // edge is very likely a non-call reference that could attribute the
            // wrong repository — see [`is_execution_edge`].
            let callees = self.call_graph.find_callees(&fqn, &discovery_query).await?;
            if let Some(r) = callees.iter().find(|r| is_execution_edge(r)) {
                r.repository_id().to_string()
            } else {
                let callers = self.call_graph.find_callers(&fqn, &discovery_query).await?;
                callers
                    .iter()
                    .find(|r| is_execution_edge(r))
                    .map(|r| r.repository_id().to_string())
                    .unwrap_or_default()
            }
        };

        // Verify the resolved symbol is actually an entry point: no *named*
        // symbol within the same repository calls it. Structural references
        // (imports, type references) and NULL-caller top-level invocations do
        // not disqualify it — the latter are what mark an entry point.
        let repo_query = CallGraphQuery::new().with_repository(&effective_repo);
        let callers_in_repo = self.call_graph.find_callers(&fqn, &repo_query).await?;
        if callers_in_repo
            .iter()
            .any(|r| is_execution_edge(r) && r.caller_symbol().is_some())
        {
            return Ok(None);
        }

        // Serve the stored feature when the repository's set has been cached;
        // fall back to a fresh BFS otherwise.
        if let Some(stored) = self.load_stored(&effective_repo).await {
            if let Some(feature) = stored.into_iter().find(|f| f.entry_point == fqn) {
                return Ok(Some(feature));
            }
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

        affected.sort_by(|a, b| b.criticality.total_cmp(&a.criticality));
        Ok(affected)
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Entry-point detection
    // ──────────────────────────────────────────────────────────────────────────

    /// Return the set of fully-qualified symbols in `repository_id` that are
    /// entry points: symbols that call at least one other symbol but are
    /// themselves never called within the repository.
    ///
    /// Only real call edges (see [`is_execution_edge`]) are considered — the
    /// bulk of a SCIP-imported graph is structural references (imports, type
    /// references) that must not be mistaken for calls, or every getter and
    /// type-referenced symbol surfaces as a spurious "entry point".
    async fn find_entry_points(&self, repository_id: &str) -> Result<Vec<String>, DomainError> {
        let all_refs = self.call_graph.find_by_repository(repository_id).await?;

        let mut callee_symbols: HashSet<String> = HashSet::new();
        let mut caller_symbols: HashSet<String> = HashSet::new();

        for r in &all_refs {
            if !is_execution_edge(r) {
                continue;
            }
            // Only a call from a *named* caller counts as "this symbol is called
            // by something in the repo". Edges with a NULL caller are top-level /
            // module-scope invocations (e.g. `app.start()` in an entry file) that
            // SCIP could not attribute to an enclosing symbol — those are exactly
            // what marks a true entry point, so they must not disqualify one.
            if let Some(caller) = r.caller_symbol() {
                caller_symbols.insert(caller.to_string());
                callee_symbols.insert(r.callee_symbol().to_string());
            }
        }

        // Entry point = calls something (has outgoing call edges) AND is never
        // called by a named symbol within this repository.
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

        visited.insert(entry_point.to_string());

        // Pre-fetch the entry-point's callees to (a) resolve its own file path
        // so the root node is not seeded with an empty string, and (b) avoid a
        // redundant call_graph lookup on the first BFS iteration. Only real call
        // edges are traversed, so reachability reflects execution, not imports.
        let initial_callees: Vec<SymbolReference> = self
            .call_graph
            .find_callees(entry_point, &query)
            .await?
            .into_iter()
            .filter(is_execution_edge)
            .collect();
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

        for reference in &initial_callees {
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

            let callees: Vec<SymbolReference> = self
                .call_graph
                .find_callees(&current, &query)
                .await?
                .into_iter()
                .filter(is_execution_edge)
                .collect();
            for reference in &callees {
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

        // ── Criticality scoring ────────────────────────────────────────────
        // A feature's importance is dominated by how much of the codebase it
        // transitively drives (reach), then how deep its call chain runs, then
        // how many files it spans.
        let reach = path.len();
        let distinct_files: HashSet<&str> = path.iter().map(|n| n.file_path.as_str()).collect();
        let file_count = distinct_files.len();
        let feature_depth = path.iter().map(|n| n.depth).max().unwrap_or(0);

        // Signal 1 — reach: distinct symbols transitively driven by this entry
        // point, normalised. This is the primary "how much of a flow is it" cue.
        let reach_score = (reach as f32 / REACH_REFERENCE).min(1.0);

        // Signal 2 — depth: normalised call-chain length.
        let depth_score = (feature_depth as f32 / DEPTH_REFERENCE).min(1.0);

        // Signal 3 — file spread: how many distinct files the flow touches,
        // normalised against reach so a broad, cross-cutting flow scores higher.
        let file_spread_score = (file_count as f32 / reach.max(1) as f32).min(1.0);

        let criticality = (WEIGHT_REACH * reach_score
            + WEIGHT_DEPTH * depth_score
            + WEIGHT_FILE_SPREAD * file_spread_score)
            .min(1.0_f32);

        let name = short_name(entry_point);
        let id = format!("{}:{}", repository_id, entry_point);

        Ok(ExecutionFeature {
            id,
            name,
            entry_point: entry_point.to_string(),
            repository_id: repository_id.to_string(),
            file_count,
            reach,
            depth: feature_depth,
            path,
            criticality,
        })
    }
}
