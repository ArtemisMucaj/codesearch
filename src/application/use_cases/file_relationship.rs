use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::application::{CallGraphUseCase, MetadataRepository, VectorRepository};
use crate::domain::{DomainError, FileEdge, FileGraph, FileGraphRepo};

/// Use case that builds a file-level dependency graph across one or more repositories.
///
/// For each repository the use case:
/// 1. Loads a symbol-name → file-path map from the vector store (chunks).
/// 2. Loads all symbol references from the call graph.
/// 3. For every reference whose callee symbol can be resolved to a file,
///    emits a weighted directed edge: `caller_file → callee_file`.
/// 4. Returns a [`FileGraph`] containing the edges, node set, and repo metadata.
pub struct FileRelationshipUseCase {
    call_graph: Arc<CallGraphUseCase>,
    vector_repo: Arc<dyn VectorRepository>,
    metadata_repo: Arc<dyn MetadataRepository>,
    /// Active namespace. The metadata store is shared across every namespace,
    /// so its `list()` returns repositories DB-wide; the graph must be confined
    /// to this namespace, otherwise a `--global` run would weld together
    /// unrelated repositories from other namespaces.
    namespace: String,
}

impl FileRelationshipUseCase {
    pub fn new(
        call_graph: Arc<CallGraphUseCase>,
        vector_repo: Arc<dyn VectorRepository>,
        metadata_repo: Arc<dyn MetadataRepository>,
        namespace: String,
    ) -> Self {
        Self {
            call_graph,
            vector_repo,
            metadata_repo,
            namespace,
        }
    }

    /// The namespace this use case is scoped to. Callers building a
    /// namespace-wide cache key (the sentinel scope id) read it from here so the
    /// key matches the repositories the graph is actually confined to.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Build the file dependency graph, scoped to the use case's default
    /// namespace. See [`Self::build_graph_in`] for the per-request variant.
    pub async fn build_graph(
        &self,
        repository_ids: Option<&[String]>,
        min_weight: usize,
        include_cross_repo: bool,
    ) -> Result<FileGraph, DomainError> {
        self.build_graph_in(repository_ids, min_weight, include_cross_repo, None)
            .await
    }

    /// Build the file dependency graph.
    ///
    /// * `repository_ids` — when `Some`, restricts the graph to the listed
    ///   repositories; `None` means "every repository in the namespace".
    /// * `min_weight` — minimum number of distinct symbol references required
    ///   before an edge is included in the result.
    /// * `include_cross_repo` — when `true`, edges whose endpoints belong to
    ///   different repositories are also included.
    /// * `namespace_override` — the namespace to confine the working set to;
    ///   `None` uses the use case's default. Lets one serve answer for any
    ///   namespace via a per-request `?namespace=` without a restart.
    pub async fn build_graph_in(
        &self,
        repository_ids: Option<&[String]>,
        min_weight: usize,
        include_cross_repo: bool,
        namespace_override: Option<&str>,
    ) -> Result<FileGraph, DomainError> {
        let namespace = namespace_override.unwrap_or(self.namespace.as_str());
        // ── 1. Resolve which repositories to analyse ───────────────────────
        // The metadata store is shared across every namespace, so `list()`
        // returns repositories DB-wide.
        let db_repos = self
            .metadata_repo
            .list()
            .await
            .map_err(|e| DomainError::storage(format!("Failed to list repositories: {e}")))?;

        // The namespace filter applies ONLY to the "all repositories" (`--global`,
        // `repository_ids == None`) case — that's what makes a global run
        // *namespace*-wide rather than *database*-wide. When explicit ids are
        // given the caller has already chosen exactly which repositories it wants
        // (e.g. a per-repository graph for a repo that may live in a *different*
        // namespace than the server's default); filtering those by namespace here
        // would silently drop them and yield an empty graph.
        let target_repos: Vec<_> = if let Some(ids) = repository_ids {
            let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
            db_repos
                .iter()
                .filter(|r| id_set.contains(r.id()))
                .cloned()
                .collect()
        } else {
            db_repos
                .iter()
                .filter(|r| r.namespace() == Some(namespace))
                .cloned()
                .collect()
        };

        // The symbol-resolution / ambiguity map (step 2) is built over the same
        // scope as the targets: for a global run that's the namespace; for an
        // explicit-id query it's the namespaces the target repos actually live in
        // (so a per-repo query still sees its own repo's symbols, and a cross-repo
        // `uses A B` still gets the namespace-wide ambiguity check via `all_repos`
        // below).
        let target_namespaces: HashSet<Option<&str>> =
            target_repos.iter().map(|r| r.namespace()).collect();
        let all_repos: Vec<_> = db_repos
            .into_iter()
            .filter(|r| target_namespaces.contains(&r.namespace()))
            .collect();

        if target_repos.is_empty() {
            return Ok(FileGraph {
                repositories: HashMap::new(),
                files: HashSet::new(),
                edges: vec![],
            });
        }

        // ── 2. Build symbol_name → (file_path, repo_id) lookup ────────────
        // Prefer the first entry when the same symbol name appears in multiple
        // files (e.g. a trait defined in a module and re-exported).
        //
        // Chunk symbol names are *bare* (a class `Order`, a free function, or a
        // member `getTotal`), but reference callees are frequently
        // qualified as `Fully\Qualified\Class#member` — 55%+ of PHP method and
        // property accesses take this form. Looking a qualified callee up by its
        // full string never matches a bare chunk key, so those references were
        // silently dropped and cross-file dependencies through methods vanished.
        //
        // To bridge the two, we resolve a `Class#member` callee by the *base
        // name* of its class part (the segment after the last `\`), which is
        // exactly what a class chunk is named. We deliberately do NOT fall back
        // to the bare member name: members like `delete`/`save`/`get` are far
        // too generic and would fabricate edges to whichever unrelated class
        // happened to define a same-named method. A basename that resolves to
        // more than one file is likewise ambiguous, so we drop it rather than
        // guess — `ambiguous` records those names.
        //
        // The map is built over every repo in the namespace (`all_repos` is
        // already namespace-scoped, see step 1), not just the `target_repos`
        // for this query. A `uses A B` call passes only {A, B},
        // but a class name unique within {A, B} can still be ambiguous
        // namespace-wide (e.g. `App\Models\Billing\Invoice` exists in both B and
        // a third repo C). Restricting the map to the query targets would hide
        // that clash and mis-bind A's reference to B's same-named class, so we
        // index all repos and let the global ambiguity check drop such names.
        let mut symbol_map: HashMap<String, (String, String)> = HashMap::new();
        let mut ambiguous: HashSet<String> = HashSet::new();
        for repo in &all_repos {
            let repo_id = repo.id().to_string();
            let entries = self.vector_repo.get_symbol_to_file_map(repo.id()).await?;
            for (sym, file) in entries {
                index_symbol(&mut symbol_map, &mut ambiguous, sym, file, &repo_id);
            }
        }

        let resolve_callee = |callee: &str| resolve_callee(callee, &symbol_map, &ambiguous);

        // ── 3. Aggregate symbol references into file-level edges ──────────
        // Key: (from_file, from_repo_id, to_file, to_repo_id)
        // Value: (weight, reference_kinds, symbols)
        let mut edge_map: HashMap<
            (String, String, String, String),
            (usize, HashSet<&'static str>, HashSet<String>),
        > = HashMap::new();

        for repo in &target_repos {
            let refs = self.call_graph.find_by_repository(repo.id()).await?;

            for sr in refs {
                let callee = sr.callee_symbol();
                let from_file = sr.caller_file_path().to_string();
                let from_repo = sr.repository_id().to_string();

                // Resolve the callee's definition file. The SCIP importer records
                // it directly in `reference_file_path` (the callee's definition
                // site, resolved across the whole index); prefer that. Fall back
                // to the chunk-derived symbol map only when the importer left the
                // reference on its own file (i.e. the callee had no definition
                // occurrence in this repo) — which lets tree-sitter-only indexes,
                // where `reference_file_path` mirrors the caller file, still
                // resolve cross-file edges the way they did before.
                let ref_file = sr.reference_file_path();
                let (to_file, to_repo): (String, String) = if ref_file != from_file {
                    (ref_file.to_string(), from_repo.clone())
                } else if let Some((f, r)) = resolve_callee(callee) {
                    (f, r)
                } else {
                    continue;
                };

                // Skip self-loops (same file on both ends).
                if from_file == to_file {
                    continue;
                }

                // Optionally skip cross-repo edges.
                if !include_cross_repo && from_repo != to_repo {
                    continue;
                }

                let key = (from_file, from_repo, to_file.clone(), to_repo.clone());
                let entry = edge_map
                    .entry(key)
                    .or_insert((0, HashSet::new(), HashSet::new()));
                entry.0 += 1;
                entry.1.insert(sr.reference_kind().as_str());
                entry.2.insert(callee.to_string());
            }
        }

        // ── 4. Materialise into FileEdge + FileGraph ──────────────────────
        let mut edges: Vec<FileEdge> = edge_map
            .into_iter()
            .filter(|(_, (w, _, _))| *w >= min_weight)
            .map(
                |((from_file, from_repo_id, to_file, to_repo_id), (weight, kinds, syms))| {
                    FileEdge {
                        from_file,
                        from_repo_id,
                        to_file,
                        to_repo_id,
                        weight,
                        reference_kinds: {
                            let mut v: Vec<String> =
                                kinds.into_iter().map(str::to_string).collect();
                            v.sort();
                            v
                        },
                        symbols: {
                            let mut v: Vec<String> = syms.into_iter().collect();
                            v.sort();
                            v
                        },
                    }
                },
            )
            .collect();

        // Deterministic order: heaviest edges first, then alphabetical.
        edges.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.from_file.cmp(&b.from_file))
                .then(a.to_file.cmp(&b.to_file))
        });

        let mut files: HashSet<String> = HashSet::new();
        for e in &edges {
            files.insert(e.from_file.clone());
            files.insert(e.to_file.clone());
        }

        let repositories: HashMap<String, FileGraphRepo> = target_repos
            .into_iter()
            .map(|r| {
                (
                    r.id().to_string(),
                    FileGraphRepo {
                        id: r.id().to_string(),
                        name: r.name().to_string(),
                        path: r.path().to_string(),
                    },
                )
            })
            .collect();

        Ok(FileGraph {
            repositories,
            files,
            edges,
        })
    }
}

/// Record one chunk symbol's definition site into the resolution maps.
///
/// The first repo to define a bare name wins the `symbol_map` slot. If a later
/// entry claims the same name from a *different* definition site, the name is
/// marked `ambiguous` and will never be resolved by base name — resolving it
/// would be a guess. The identity that distinguishes sites is `(file, repo)`,
/// not `file` alone: services routinely share a relative path (e.g. every
/// service has its own `test/Framework/Client.php`), so comparing paths only
/// would wrongly treat two repos' `Client` as one definition and leak a caller's
/// own symbol onto a foreign repo's file.
fn index_symbol(
    symbol_map: &mut HashMap<String, (String, String)>,
    ambiguous: &mut HashSet<String>,
    sym: String,
    file: String,
    repo_id: &str,
) {
    match symbol_map.get(&sym) {
        Some((f, r)) if (f.as_str(), r.as_str()) != (file.as_str(), repo_id) => {
            ambiguous.insert(sym);
        }
        Some(_) => {}
        None => {
            symbol_map.insert(sym, (file, repo_id.to_string()));
        }
    }
}

/// Split a `Class#member` callee's class part into its namespace segments and
/// its base name: `App\Models\Orders\Order#get` → (`["App","Models","Orders"]`,
/// `"Order"`). Returns `None` for a callee with no `#` or an empty class part
/// (a bare symbol, which resolves by exact match instead).
fn class_path(callee: &str) -> Option<(Vec<&str>, &str)> {
    let class = callee.split_once('#')?.0;
    let mut segs: Vec<&str> = class
        .split(['\\', '/', ':'])
        .filter(|s| !s.is_empty())
        .collect();
    let base = segs.pop()?;
    Some((segs, base))
}

/// Resolve a reference callee to the `(file, repo)` that defines it, using, in
/// order:
///   1. an exact chunk-symbol match (a bare class or free function), then
///   2. the base name of a `Class#member` callee's class — but only when the
///      base name is unambiguous (defined in a single file across the target
///      repos) AND the callee's immediate parent namespace segment appears in
///      the resolved file's path.
///
/// The namespace check is what stops a shared base name from producing a false
/// edge: `App\Reports\DeleteRequest#create` and a shared library's
/// `App\Consents\DeleteRequest` share the base `DeleteRequest`, but the parent
/// segment `Reports` is absent from `src/Consents/DeleteRequest.php`, so the
/// basename match is rejected rather than guessed. Bare member fallback is
/// deliberately omitted: members like `get`/`save`/`delete` are too generic to
/// resolve by name.
fn resolve_callee(
    callee: &str,
    symbol_map: &HashMap<String, (String, String)>,
    ambiguous: &HashSet<String>,
) -> Option<(String, String)> {
    if let Some(hit) = symbol_map.get(callee) {
        return Some(hit.clone());
    }
    let (namespace, base) = class_path(callee)?;
    if ambiguous.contains(base) {
        return None;
    }
    let (file, repo) = symbol_map.get(base)?;
    // Vendor/framework roots (the top-level namespace) are too broad to
    // discriminate, so only the immediate parent segment must be reflected in
    // the target path.
    if let Some(parent) = namespace.last() {
        if !file.contains(parent) {
            return None;
        }
    }
    Some((file.clone(), repo.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc_in(file: &str, repo: &str) -> (String, String) {
        (file.to_string(), repo.to_string())
    }

    // All `fixture()` definitions live in the `lib` repo.
    fn loc(file: &str) -> (String, String) {
        loc_in(file, "lib")
    }

    /// Build the resolution maps through the real [`index_symbol`] logic from a
    /// list of `(symbol, file, repo)` definition sites.
    fn build(defs: &[(&str, &str, &str)]) -> (HashMap<String, (String, String)>, HashSet<String>) {
        let mut m = HashMap::new();
        let mut amb = HashSet::new();
        for (sym, file, repo) in defs {
            index_symbol(&mut m, &mut amb, sym.to_string(), file.to_string(), repo);
        }
        (m, amb)
    }

    fn fixture() -> (HashMap<String, (String, String)>, HashSet<String>) {
        build(&[
            ("Order", "src/Models/Orders/Order.php", "lib"),
            ("GenericUtils", "GenericUtils.php", "lib"),
            // Defined once → resolvable, but the namespace check must still
            // reject a foreign caller (see the DeleteRequest tests below).
            ("DeleteRequest", "src/Consents/DeleteRequest.php", "lib"),
            // Same base name in several files → ambiguous, must never resolve.
            ("Invoice", "src/Models/Billing/Invoice.php", "lib"),
            ("Invoice", "src/Models/Archive/Invoice.php", "lib"),
        ])
    }

    #[test]
    fn same_relative_path_in_two_repos_is_ambiguous() {
        // `test/Framework/Client.php` exists in both svc-a and svc-b. The bare
        // name `Client` must be flagged ambiguous, not silently bound to
        // whichever repo was indexed first.
        let (_m, amb) = build(&[
            ("Client", "test/Framework/Client.php", "svc-a"),
            ("Client", "test/Framework/Client.php", "svc-b"),
        ]);
        assert!(amb.contains("Client"));
    }

    #[test]
    fn same_site_indexed_twice_is_not_ambiguous() {
        let (m, amb) = build(&[("Utils", "Utils.php", "lib"), ("Utils", "Utils.php", "lib")]);
        assert!(!amb.contains("Utils"));
        assert_eq!(m.get("Utils"), Some(&loc_in("Utils.php", "lib")));
    }

    #[test]
    fn exact_bare_symbol_resolves() {
        let (m, amb) = fixture();
        assert_eq!(
            resolve_callee("GenericUtils", &m, &amb),
            Some(loc("GenericUtils.php"))
        );
    }

    #[test]
    fn qualified_member_resolves_by_class_base_name() {
        let (m, amb) = fixture();
        assert_eq!(
            resolve_callee("App\\Models\\Orders\\Order#getTotal", &m, &amb),
            Some(loc("src/Models/Orders/Order.php"))
        );
    }

    #[test]
    fn foreign_namespace_with_shared_base_name_is_rejected() {
        let (m, amb) = fixture();
        // Caller's `Reports\DeleteRequest` must NOT resolve to the library's
        // `Consents/DeleteRequest.php` — parent segment `Reports` is absent.
        assert_eq!(
            resolve_callee("App\\Reports\\DeleteRequest#create", &m, &amb),
            None
        );
    }

    #[test]
    fn matching_namespace_with_shared_base_name_resolves() {
        let (m, amb) = fixture();
        assert_eq!(
            resolve_callee("App\\Consents\\DeleteRequest#create", &m, &amb),
            Some(loc("src/Consents/DeleteRequest.php"))
        );
    }

    #[test]
    fn ambiguous_base_name_never_resolves() {
        let (m, amb) = fixture();
        assert_eq!(
            resolve_callee("App\\Models\\Billing\\Invoice#get", &m, &amb),
            None
        );
    }

    #[test]
    fn unknown_callee_returns_none() {
        let (m, amb) = fixture();
        assert_eq!(resolve_callee("Nonexistent\\Thing#foo", &m, &amb), None);
    }
}
