# Plan: Execution Features, Cluster Detection & Expanded MCP Surface

Inspired by [code-review-graph](https://github.com/tirth8205/code-review-graph). This document captures the approach for three related features that build on the existing call graph and file-graph infrastructure.

---

## Table of Contents

1. [Execution Features with Criticality Scoring](#1-execution-features-with-criticality-scoring)
2. [Cluster Detection](#2-cluster-detection)
3. [Expanded MCP Tool Surface](#3-expanded-mcp-tool-surface)

---

## 1. Execution Features with Criticality Scoring

### What it is

An **execution feature** is a named, ordered call chain starting from an entry-point symbol (a function with zero callers, or one matching a well-known pattern like `main`, `run`, `handle_*`, framework route handlers) and traced forward through callees up to a configurable depth. Each feature receives a **criticality score** (0.0–1.0) combining several weighted signals so that the most important paths surface first.

This complements the existing `ImpactAnalysisUseCase` (which walks *upward* through callers) by adding a *downward* forward-trace that answers: "what does this entry point actually do and how critical is it?"

### New domain models

**`src/domain/models/feature.rs`**

```rust
pub struct FeatureNode {
    pub symbol: String,
    pub file_path: String,
    pub line: u32,
    pub depth: usize,
    pub repository_id: String,
}

pub struct ExecutionFeature {
    pub id: String,                  // UUID, stable across re-runs
    pub name: String,                // human-readable label (entry-point symbol short name)
    pub entry_point: String,         // fully-qualified entry-point symbol
    pub repository_id: String,
    pub path: Vec<FeatureNode>,         // ordered call chain, entry point at index 0
    pub depth: usize,                // len(path) - 1
    pub file_count: usize,           // distinct files touched
    pub criticality: f32,            // 0.0–1.0, see scoring below
}
```

Export from `src/domain/models/mod.rs`.

### New use case

**`src/application/use_cases/execution_features.rs`**

The use case takes a `CallGraphUseCase` and a `VectorRepository` (needed to check whether a symbol has test coverage).

**Entry-point detection** — a symbol is an entry point when:
- It has zero callers in the call graph (`find_callers` returns empty).
- Its short name matches any of: `main`, `run`, `start`, `init`, `handle`, `execute`, `process`, test function prefixes (`test_*`, `it_*`), or common framework decorator patterns stored as constants.

**Forward BFS with cycle detection** — from an entry point, follow `find_callees` and collect `FeatureNode` objects in BFS order. A `visited: HashSet<String>` tracks every symbol enqueued. Before enqueuing a callee, check `visited.contains(callee)` — if true, skip it. This mirrors the pattern already used by `ImpactAnalysisUseCase` and `SymbolContextUseCase`, and handles all graph shapes including mutual recursion and diamond dependencies. No artificial depth cap is needed: BFS terminates naturally once every reachable symbol has been visited.

**Criticality scoring** — compute five independent sub-scores then sum and clamp to 1.0:

| Signal | Weight | How to compute |
|---|---|---|
| File spread | 0.30 | `(distinct_files as f32 / total_nodes as f32).min(1.0)` |
| Security sensitivity | 0.25 | 1.0 if any node's symbol contains auth/crypto/validate/password/token/secret/permission, else 0.0 |
| External calls | 0.20 | fraction of callees that could not be resolved to an indexed symbol |
| Test coverage gap | 0.15 | 0.30 if no `test_*` symbol is a direct or indirect caller of the entry point, else 0.05 |
| Depth | 0.10 | `(feature.depth as f32 / 20.0_f32).min(1.0)` — normalised against a soft reference depth of 20 |

**Public API:**

```rust
impl ExecutionFeaturesUseCase {
    /// Detect all entry points for a repository and compute their features.
    pub async fn list_features(
        &self,
        repository_id: &str,
        limit: usize,
    ) -> Result<Vec<ExecutionFeature>, DomainError>;

    /// Retrieve a single feature by entry-point symbol name.
    pub async fn get_feature(
        &self,
        symbol: &str,
        repository_id: Option<&str>,
    ) -> Result<Option<ExecutionFeature>, DomainError>;

    /// Given a set of changed symbols, return the features they participate in,
    /// sorted by descending criticality.
    pub async fn get_affected_features(
        &self,
        changed_symbols: &[String],
        repository_id: Option<&str>,
    ) -> Result<Vec<ExecutionFeature>, DomainError>;
}
```

### Storage

Features are computed on demand — no persistence needed for an initial implementation. They can be memoized in an `Arc<Mutex<HashMap<String, Vec<ExecutionFeature>>>>` keyed by `repository_id` if re-computation latency becomes a problem. Persist to a `features` DuckDB table (columns: id, entry_point, repository_id, criticality, serialized_path JSON) only if caching becomes necessary.

### Wiring

- Add `ExecutionFeaturesUseCase` to `Container` (`src/connector/api/container.rs`) — it depends on the already-wired `call_graph_use_case` and `vector_repo`.
- Add a `features` CLI command in `src/cli/` with subcommands `list`, `get`, `affected`.
- Route it in `src/connector/api/router.rs`.

---

## 2. Cluster Detection

### What it is

Group the file-level dependency graph (`FileGraph`) into named **clusters** — groups of files that are tightly coupled to each other and loosely coupled to the rest. Clusters are the foundation for architecture overviews, smarter search boosting, and risk scoring (a change that crosses cluster boundaries is riskier than one confined within one).

### Louvain vs Leiden — and why Leiden wins for code graphs

code-review-graph uses the **Leiden algorithm** (via a Python igraph binding). The original plan suggested Louvain, but Leiden is the right choice here. Here is why.

**Louvain (2008)** runs in two phases: (1) greedily move nodes to neighbouring clusters to maximise modularity, (2) aggregate each cluster into a super-node and repeat. It is fast and widely used, but has a known structural defect: it can produce **disconnected clusters** — nodes assigned to the same cluster that have no path between them in the graph. For a code dependency graph, a cluster whose files cannot reach each other via imports or calls is meaningless as an architectural unit.

**Leiden (2019)** inserts a **refinement phase** between local moving and aggregation. During refinement, each node is allowed to move to a random subset of neighbouring clusters rather than just the best one, which lets the algorithm escape local optima and guarantees that every resulting cluster is **internally connected**. In practice this produces tighter, more semantically coherent clusters, which is exactly what matters when the goal is to name architectural boundaries.

**code-review-graph's workaround**: because their implementation runs in Python with igraph, they cap Leiden at `n_iterations=2` and skip the recursive sub-cluster splitting pass to avoid exponential blow-up on large repos. In Rust these constraints are unnecessary — a native Leiden implementation runs 20–50× faster than the Python binding, so the full algorithm can be used without iteration caps.

### Algorithm choice

Use **Leiden** implemented in pure Rust on top of `petgraph` for graph representation:

```toml
petgraph = "0.6"
```

There is no production-ready Leiden crate for Rust yet, so the implementation lives inside `src/application/use_cases/cluster_detection.rs`. It is roughly 300 lines following the original Traag et al. (2019) paper: local moving → refinement → aggregation, repeat until modularity gain is below `1e-6` or 50 iterations are reached.

**Fallback**: when a repository has fewer than 10 file nodes, skip clustering and assign each file to its own cluster (graph is too small to be meaningful).

### What to adopt from code-review-graph

Three concrete improvements over a naïve Leiden implementation:

**1. Differentiated edge weights by reference kind**

code-review-graph does not treat all edges equally. It assigns weights based on relationship type, which guides the algorithm to cluster files that share strong semantic bonds:

| Reference kind | Weight |
|---|---|
| Call / MethodCall | 1.0 |
| Inheritance | 0.8 |
| Implementation | 0.7 |
| TypeReference | 0.6 |
| Import | 0.5 |
| Unknown | 0.3 |

The `FileEdge` domain model already carries `reference_kinds: Vec<String>`. During graph construction, compute a composite edge weight: `base_weight × Σ(kind_weight for kind in reference_kinds) / reference_kinds.len()`, where `base_weight` is the existing `FileEdge::weight` (distinct symbol count). This makes the algorithm cluster files by how they *relate*, not just how often they reference each other.

**2. O(edges) batch cohesion computation**

A naïve cohesion metric iterates over all edges for each cluster — O(edges × clusters). code-review-graph's batch approach is O(edges) total: build a single `qualified_name → cluster_index` map, then walk the edge list once, classifying each edge as internal (both endpoints in the same cluster) or external. Adopt this exactly:

```
cohesion = internal_edges / (internal_edges + external_edges)
```

**3. Four-step cluster naming heuristic**

"Longest common path prefix" (the original plan) degrades to a useless root prefix on flat repositories. code-review-graph's heuristic is more robust:

1. Extract the most common short directory / module name among member files.
2. If one class name accounts for >40% of the member symbols, use that name instead.
3. Otherwise, extract the most frequent meaningful keywords from member symbol names (strip common words: get, set, test, new, is, has).
4. Combine as `"{dir}-{keyword}"`, slug-cased, max 30 characters.

Implement `fn name_cluster(members: &[String], symbol_map: &HashMap<String, String>) -> String` using the existing `Language::from_path` for extension detection and the same camelCase/snake_case split already used in the tree-sitter parser.

### New domain models

**`src/domain/models/cluster.rs`**

```rust
pub struct Cluster {
    pub id: String,              // UUID
    pub name: String,            // heuristic: longest common path prefix of member files
    pub repository_id: String,
    pub dominant_language: String,
    pub size: usize,             // number of member files
    pub cohesion: f32,           // internal_edges / possible_internal_edges
    pub members: Vec<String>,    // file paths sorted alphabetically
}

pub struct ClusterGraph {
    pub clusters: Vec<Cluster>,
    pub repository_id: String,
    pub total_files: usize,
    pub total_edges: usize,
}
```

### New use case

**`src/application/use_cases/cluster_detection.rs`**

Depends on `FileRelationshipUseCase` to obtain the `FileGraph`.

**Steps:**

1. Call `FileRelationshipUseCase::build_graph` with `min_weight = 1` and `include_cross_repo = false`.
2. Convert `FileGraph` edges into a `petgraph::Graph<String, usize>` (undirected, edge weight = `FileEdge::weight`).
3. Run Leiden cluster detection — iterate until modularity gain is below `1e-6` or 50 iterations are reached.
4. Map each partition back to `Cluster` objects:
   - **Name**: longest common directory path prefix of member files; fall back to the most-referenced file's parent directory name.
   - **Dominant language**: most common `Language` among members (detected from file extensions via the existing `Language::from_path`).
   - **Cohesion**: `actual_internal_edges / (n * (n-1) / 2)` where n = cluster size.
5. Sort clusters by descending size.

**Public API:**

```rust
impl ClusterDetectionUseCase {
    pub async fn detect(
        &self,
        repository_id: &str,
    ) -> Result<ClusterGraph, DomainError>;

    /// Return the cluster a given file belongs to.
    pub async fn cluster_for_file(
        &self,
        file_path: &str,
        repository_id: &str,
    ) -> Result<Option<Cluster>, DomainError>;

    /// Return a high-level architecture summary: one paragraph per cluster
    /// listing its name, size, dominant language, and top outgoing dependencies.
    pub async fn architecture_overview(
        &self,
        repository_id: &str,
    ) -> Result<String, DomainError>;
}
```

The `architecture_overview` method is pure text assembly from `ClusterGraph` data — no LLM call needed. Format: a Markdown table with one row per cluster (name, files, language, top 3 dependencies by edge weight to other clusters).

### Storage

Communities are also cheap to recompute (they derive from the call graph which is already persisted). No additional storage is needed unless cache-on-index is desired later. If that becomes necessary, add a `clusters` table to the shared DuckDB connection using the same pattern as `DuckdbCallGraphRepository`.

### Wiring

- Add `ClusterDetectionUseCase` to `Container` — depends on `file_graph_use_case()` and `metadata_repository()`.
- Add a `clusters` CLI command with subcommands `list`, `get <file>`, `overview`.
- Route in `router.rs`.

---

## 3. Expanded MCP Tool Surface

The current MCP server (`src/connector/adapter/mcp/server.rs`) exposes 3 tools. The target is 8 tools, adding the 5 below. Each new tool follows the existing `#[tool]` macro pattern; input structs derive `Deserialize + JsonSchema`; output is `serde_json::to_string_pretty` of the domain type.

### New tools overview

| Tool name | Depends on | What it returns |
|---|---|---|
| `list_features` | `ExecutionFeaturesUseCase` | Top-N features sorted by criticality |
| `get_feature` | `ExecutionFeaturesUseCase` | Single feature with full call chain |
| `get_affected_features` | `ExecutionFeaturesUseCase` | Features impacted by a list of changed symbols |
| `list_clusters` | `ClusterDetectionUseCase` | All clusters for a repository |
| `get_architecture_overview` | `ClusterDetectionUseCase` | Markdown architecture summary |
| `query_graph` | `CallGraphUseCase` (direct) | Nodes matching one named relationship pattern |

### Tool specifications

#### `list_features`

```rust
struct ListFeaturesInput {
    repository_id: String,
    /// Maximum results (default 20, cap 100).
    limit: usize,
}
// Returns: Vec<ExecutionFeature> as JSON, sorted by criticality desc.
```

Typical AI use: "Show me the most critical execution paths before I refactor this module."

#### `get_feature`

```rust
struct GetFeatureInput {
    /// Entry-point symbol name (substring match, same resolution as impact analysis).
    symbol: String,
    repository_id: Option<String>,
}
// Returns: Option<ExecutionFeature> as JSON (null when not found).
```

#### `get_affected_features`

```rust
struct AffectedFeaturesInput {
    /// Symbols that changed (e.g. function names from a diff).
    changed_symbols: Vec<String>,
    repository_id: Option<String>,
    /// Maximum results (default 10, cap 50).
    limit: usize,
}
// Returns: Vec<ExecutionFeature>, sorted by criticality desc.
```

Typical AI use: "I changed these three functions — which execution features are now at risk?"

#### `list_clusters`

```rust
struct ListClustersInput {
    repository_id: String,
}
// Returns: ClusterGraph as JSON.
```

Typical AI use: "What are the architectural layers of this codebase?"

#### `get_architecture_overview`

```rust
struct ArchitectureOverviewInput {
    repository_id: String,
}
// Returns: String (Markdown table).
```

Typical AI use: "Give me a one-page architecture overview before I start this large refactor."

### Changes to `server.rs`

- Add the six new input structs above (`ListFeaturesInput`, `GetFeatureInput`, `AffectedFeaturesInput`, `ListClustersInput`, `ArchitectureOverviewInput`, `QueryGraphInput`) plus the `GraphQueryResult` / `GraphQueryNode` output types.
- Add six new `#[tool(...)]` methods to `CodesearchMcpServer`.
- `Container` must expose `execution_features_use_case()` and `cluster_detection_use_case()` factory methods (following the same pattern as `impact_use_case()`). `query_graph` calls `call_graph_use_case()` directly — no new factory method needed.
- Update the `instructions` string in `get_info()` to list all 9 tools.

#### `query_graph`

`query_graph` is the most ergonomic of all the new tools and, paradoxically, the cheapest to build: **zero new infrastructure**. Every pattern it supports maps directly onto `CallGraphUseCase` methods + a `reference_kind` filter that already exists on `CallGraphQuery`.

The value is not new data — it is a clean, intention-named vocabulary. Today an AI using `get_symbol_context` receives *all* callers and *all* callees at once, including noise (imports, type references, variable reads). If it wants to know "who inherits from this class", it must receive everything and filter mentally. `query_graph` asks for exactly one relationship type and returns only that.

**Supported patterns** (directly from code-review-graph's `query.py`):

| Pattern | Maps to | `reference_kind` filter |
|---|---|---|
| `callers_of` | `find_callers(symbol)` | none (all kinds) |
| `callees_of` | `find_callees(symbol)` | none (all kinds) |
| `imports_of` | `find_callees(symbol)` | `Import` |
| `importers_of` | `find_callers(symbol)` | `Import` |
| `inheritors_of` | `find_callers(symbol)` | `Inheritance`, `Implementation` |
| `children_of` | `find_callees(symbol)` | `Inheritance`, `Implementation` |
| `tests_for` | `find_callers(symbol)` | post-filter: caller symbol matches `test_*` / `*_test` / `*_spec`, or file path contains `test`/`spec` |
| `file_summary` | `find_by_file(file_path)` | none |

All 8 `ReferenceKind` variants needed (`Import`, `Inheritance`, `Implementation`) already exist in the domain model.

**Input / output:**

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryGraphInput {
    /// One of: callers_of, callees_of, imports_of, importers_of,
    /// inheritors_of, children_of, tests_for, file_summary
    pub pattern: String,
    /// Symbol name or file path, depending on pattern.
    /// Resolved with the same substring-match fallback as analyze_impact.
    pub target: String,
    pub repository_id: Option<String>,
    /// Maximum results (default 50, cap 500).
    #[serde(default = "default_query_limit")]
    pub limit: usize,
}

// Output — one entry per unique symbol (deduped from individual reference sites):
pub struct GraphQueryNode {
    pub symbol: String,
    pub file_path: String,
    pub line: u32,
    pub reference_kind: String,
    pub repository_id: String,
}

pub struct GraphQueryResult {
    pub pattern: String,
    pub target: String,
    pub nodes: Vec<GraphQueryNode>,
    pub total: usize,
}
```

Deduplicating by `symbol` (keeping the first location per symbol) prevents flooding the AI with 40 rows for the same callee called from 40 sites.

**Implementation**: No new use case. The `#[tool(name = "query_graph")]` method in `server.rs` calls `container.call_graph_use_case()` directly, matches on `input.pattern`, and dispatches to the appropriate `CallGraphUseCase` method with the right `CallGraphQuery::with_reference_kind(...)` chain. Approximately 100 lines total including the output mapping.

**Concrete AI use-cases this unlocks:**

- *"Before I change `IRepository`, who implements it?"* → `inheritors_of(IRepository)`
- *"Is `authenticate` covered by tests?"* → `tests_for(authenticate)` → empty means no coverage
- *"What does `UserService` import — I want to understand its dependencies"* → `imports_of(UserService)`
- *"Which modules depend on this file I'm about to restructure?"* → `importers_of(src/auth/utils.rs)`
- *"What's the full relationship picture of this file before I split it?"* → `file_summary(src/auth/service.rs)`

**`tests_for` is particularly useful**: it surfaces the test gap that the criticality scorer in execution features approximates heuristically. With `query_graph`, an AI can ask it explicitly and get zero results as a concrete signal that a function has no test coverage at all.

This tool upgrades `get_symbol_context` from "give me everything" to a vocabulary of precise questions. The target count becomes **9 MCP tools** (5 original + 4 new = 9, treating `query_graph` as a full tool alongside the feature/cluster tools).

### Future tools (not in scope now, but designed to fit)

| Tool | Notes |
|---|---|
| `find_large_functions` | Query `vector_repo` for chunks where `end_line - start_line > threshold`. Straightforward. |
| `cross_repo_search` | `SearchCodeUseCase` already supports `with_repositories`; just needs a dedicated MCP input that makes multi-repo explicit. |

---

## Implementation Order

1. **`query_graph` MCP tool** — zero new infrastructure; ~100 lines directly in `server.rs`. Ships first, independently. Immediately improves AI ergonomics over the existing `get_symbol_context`.
2. **Execution features** — builds only on existing `CallGraphUseCase`. No new dependencies. Add domain model → use case → container method → 3 MCP tools.
3. **Cluster detection** — add `petgraph`, implement Leiden, build `ClusterDetectionUseCase` on top of existing `FileRelationshipUseCase` → container → 2 MCP tools.

Each step is independently shippable. `query_graph` has no dependency on the other two.
