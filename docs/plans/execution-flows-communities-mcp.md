# Plan: Execution Features, Community Detection & Expanded MCP Surface

Inspired by [code-review-graph](https://github.com/tirth8205/code-review-graph). This document captures the approach for three related features that build on the existing call graph and file-graph infrastructure.

---

## Table of Contents

1. [Execution Features with Criticality Scoring](#1-execution-features-with-criticality-scoring)
2. [Community Detection](#2-community-detection)
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

**Forward BFS** — from an entry point, follow `find_callees` up to `MAX_FEATURE_DEPTH = 15` hops. Use a visited set to prevent cycles. Collect `FeatureNode` objects in BFS order.

**Criticality scoring** — compute five independent sub-scores then sum and clamp to 1.0:

| Signal | Weight | How to compute |
|---|---|---|
| File spread | 0.30 | `(distinct_files / MAX_FEATURE_DEPTH).min(1.0)` |
| Security sensitivity | 0.25 | 1.0 if any node's symbol contains auth/crypto/validate/password/token/secret/permission, else 0.0 |
| External calls | 0.20 | fraction of callees that could not be resolved to an indexed symbol |
| Test coverage gap | 0.15 | 0.30 if no `test_*` symbol is a direct or indirect caller of the entry point, else 0.05 |
| Depth | 0.10 | `(feature.depth / MAX_FEATURE_DEPTH as f32).min(1.0)` |

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

## 2. Community Detection

### What it is

Group the file-level dependency graph (`FileGraph`) into named **communities** — clusters of files that are tightly coupled to each other and loosely coupled to the rest. Communities are the foundation for architecture overviews, smarter search boosting, and risk scoring (a change that crosses community boundaries is riskier than one confined within one).

### Algorithm choice

Use the **Louvain algorithm** from the `petgraph` crate, which is already a natural fit:

- `petgraph` is pure Rust, no native dependencies.
- The `FileGraph` already gives us the undirected weighted edge list needed as input.
- Louvain is O(n log n) in practice and handles repos with hundreds of thousands of files.

Add to `Cargo.toml`:
```toml
petgraph = "0.6"
```

There is no need for the Leiden algorithm (which code-review-graph uses via a Python igraph binding). Louvain produces comparable quality and is trivial to integrate.

**Fallback**: when a repository has fewer than 10 file nodes, skip clustering and assign each file to its own community (graph is too small to be meaningful).

### New domain models

**`src/domain/models/community.rs`**

```rust
pub struct Community {
    pub id: String,              // UUID
    pub name: String,            // heuristic: longest common path prefix of member files
    pub repository_id: String,
    pub dominant_language: String,
    pub size: usize,             // number of member files
    pub cohesion: f32,           // internal_edges / possible_internal_edges
    pub members: Vec<String>,    // file paths sorted alphabetically
}

pub struct CommunityGraph {
    pub communities: Vec<Community>,
    pub repository_id: String,
    pub total_files: usize,
    pub total_edges: usize,
}
```

### New use case

**`src/application/use_cases/community_detection.rs`**

Depends on `FileRelationshipUseCase` to obtain the `FileGraph`.

**Steps:**

1. Call `FileRelationshipUseCase::build_graph` with `min_weight = 1` and `include_cross_repo = false`.
2. Convert `FileGraph` edges into a `petgraph::Graph<String, usize>` (undirected, edge weight = `FileEdge::weight`).
3. Run Louvain community detection — iterate until modularity stops improving or a max iteration cap (50) is reached.
4. Map each partition back to `Community` objects:
   - **Name**: longest common directory path prefix of member files; fall back to the most-referenced file's parent directory name.
   - **Dominant language**: most common `Language` among members (detected from file extensions via the existing `Language::from_path`).
   - **Cohesion**: `actual_internal_edges / (n * (n-1) / 2)` where n = community size.
5. Sort communities by descending size.

**Public API:**

```rust
impl CommunityDetectionUseCase {
    pub async fn detect(
        &self,
        repository_id: &str,
    ) -> Result<CommunityGraph, DomainError>;

    /// Return the community a given file belongs to.
    pub async fn community_for_file(
        &self,
        file_path: &str,
        repository_id: &str,
    ) -> Result<Option<Community>, DomainError>;

    /// Return a high-level architecture summary: one paragraph per community
    /// listing its name, size, dominant language, and top outgoing dependencies.
    pub async fn architecture_overview(
        &self,
        repository_id: &str,
    ) -> Result<String, DomainError>;
}
```

The `architecture_overview` method is pure text assembly from `CommunityGraph` data — no LLM call needed. Format: a Markdown table with one row per community (name, files, language, top 3 dependencies by edge weight to other communities).

### Storage

Communities are also cheap to recompute (they derive from the call graph which is already persisted). No additional storage is needed unless cache-on-index is desired later. If that becomes necessary, add a `communities` table to the shared DuckDB connection using the same pattern as `DuckdbCallGraphRepository`.

### Wiring

- Add `CommunityDetectionUseCase` to `Container` — depends on `file_graph_use_case()` and `metadata_repository()`.
- Add a `communities` CLI command with subcommands `list`, `get <file>`, `overview`.
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
| `list_communities` | `CommunityDetectionUseCase` | All communities for a repository |
| `get_architecture_overview` | `CommunityDetectionUseCase` | Markdown architecture summary |

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

#### `list_communities`

```rust
struct ListCommunitiesInput {
    repository_id: String,
}
// Returns: CommunityGraph as JSON.
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

- Add the five new input structs above.
- Add five new `#[tool(...)]` methods to `CodesearchMcpServer`.
- `Container` must expose `execution_features_use_case()` and `community_detection_use_case()` factory methods (following the same pattern as `impact_use_case()`).
- Update the `instructions` string in `get_info()` to list all 8 tools.

### Future tools (not in scope now, but designed to fit)

| Tool | Notes |
|---|---|
| `detect_changes` | Risk-scored diff analysis. Needs feature + community data — build after both are stable. Combine `get_affected_features` criticality with caller-count and cross-community crossing penalty. |
| `find_large_functions` | Query `vector_repo` for chunks where `end_line - start_line > threshold`. Straightforward. |
| `query_graph` | Unified graph query: callers / callees / tests / imports / inheritance in one call. Wraps existing `CallGraphUseCase`. |
| `cross_repo_search` | `SearchCodeUseCase` already supports `with_repositories`; just needs a dedicated MCP input that makes multi-repo explicit. |

---

## Implementation Order

1. **Execution features** — builds only on existing `CallGraphUseCase`. No new dependencies. Add domain model → use case → container method → 3 MCP tools.
2. **Community detection** — add `petgraph`, build `CommunityDetectionUseCase` on top of existing `FileRelationshipUseCase` → container → 2 MCP tools.
3. **`detect_changes` tool** — combines both; implement last once features and communities are stable.

Each step is independently shippable: the MCP tools for features can be released before community detection exists, since they have no shared dependencies.
