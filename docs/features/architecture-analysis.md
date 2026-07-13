# Architecture & Dependency Analysis

In addition to per-symbol call graph queries (`impact`, `context`, `explain` — see
[Call Graph Analysis](./call-graph.md)), CodeSearch analyses the **file- and
repository-level** dependency graph built during indexing. These commands answer
"how is this codebase structured?" rather than "where is X?".

All three commands derive from the same `SymbolReference` edges populated during
`codesearch index`, so re-index after code changes to keep the analysis current.

## Result caching

Detected clusters, symbol communities, and execution features are persisted in
the DuckDB database (`analysis_runs`, `clusters`, `cluster_members`,
`execution_features`, `execution_feature_nodes` tables) the first time they are
computed. Subsequent queries are served from storage instead of re-running graph
construction and Leiden detection.

Read-only commands (`features`, `visualize`) open the database without holding
the exclusive write lock so multiple processes can run concurrently. They still
fill the cache via a short-lived **deferred write-back** connection opened only
for the flush: the freshly computed result is persisted for the *next*
invocation while the current run keeps serving from what it just computed. A
write-back that can't get the lock (e.g. a concurrent `index` holds it) is
skipped — it only costs the next run its warm start, never correctness.
Writable contexts (`clusters`, `symbol-clusters`, the MCP server) persist
in-place through their existing write connection.

The stored results are invalidated automatically whenever `codesearch index`
changes the call graph they were computed from, and removed by
`codesearch delete`.

## Execution Features (`codesearch features`)

An **execution feature** is a forward call chain rooted at an entry-point symbol — a
self-contained slice of behaviour that the codebase exposes. Each feature is assigned a
**criticality** score derived from how deep and how wide its call chain is, so the most
load-bearing flows surface first.

### Subcommands

```bash
# List entry-point features for a repository, sorted by descending criticality
codesearch features list my-repo

# Cap the number of features and emit JSON
codesearch features list my-repo --limit 10 --format json

# Show the execution feature for a single entry-point symbol (exact or substring)
codesearch features get handle_request

# Show the features impacted by changing one or more symbols
codesearch features impacted authenticate hash_password
```

### Options

| Flag | Subcommand | Default | Description |
|------|------------|---------|-------------|
| `-l, --limit` | `list` | `20` | Maximum number of features to return |
| `-r, --repository` | `get`, `impacted` | (none) | Restrict lookup to a specific repository ID |
| `-F, --format` | all | `text` | Output format: `text`, `json`, or `vimgrep` |

### Example: `features list`

```text
Execution Features (3 total)
─────────────────────────────────────────
login_flow  criticality=0.91  depth=4  files=6
  entry: handle_login

index_repository  criticality=0.74  depth=5  files=9
  entry: run_index

search_flow  criticality=0.68  depth=3  files=4
  entry: handle_search
```

### Example: `features get`

```text
Execution Feature: login_flow
─────────────────────────────────────────
Entry point : handle_login
Repository  : my-api
Criticality : 0.91
Depth       : 4
Files       : 6

Call chain:
handle_login
    └── authenticate [src/auth/mod.rs:10]
        └── verify_password [src/crypto/hash.rs:22]
        └── generate_token [src/crypto/token.rs:7]
```

## Clusters (`codesearch clusters`)

The `clusters` command runs the [Leiden](https://en.wikipedia.org/wiki/Leiden_algorithm)
community-detection algorithm over the file-level call graph to identify groups of
tightly-coupled files — i.e. architectural modules — even when those groupings are not
reflected in the directory layout.

### Subcommands

```bash
# List all clusters detected in the repository
codesearch clusters list my-repo

# JSON output
codesearch clusters list my-repo --format json

# Show which cluster a specific file belongs to (path as indexed, repo-relative)
codesearch clusters get src/api/auth.rs my-repo

# Print a high-level Markdown architecture overview table
codesearch clusters overview my-repo
```

### Options

| Flag | Subcommand | Default | Description |
|------|------------|---------|-------------|
| `-F, --format` | `list`, `get` | `text` | Output format: `text` or `json` (vimgrep is not supported) |

> The `overview` subcommand always emits a Markdown table and takes no `--format` flag.

### Example: `clusters list`

```text
Clusters for `my-repo` — 3 clusters, 42 files, 118 edges
────────────────────────────────────────────────────
  1. auth (8 files, rust, cohesion 0.82)
      src/auth/mod.rs
      src/crypto/hash.rs
      src/crypto/token.rs
      src/db/users.rs
      src/middleware/session.rs
      … and 3 more
  2. indexing (12 files, rust, cohesion 0.77)
      src/connector/adapter/duckdb/vector.rs
      …
```

### Example: `clusters get`

```text
File `src/api/auth.rs` belongs to cluster `auth` (8 files, rust, cohesion 0.82)
```

## Symbol Communities (`codesearch symbol-clusters`)

`symbol-clusters` runs the **same Leiden algorithm one level finer** — over the
symbol call graph (`symbol_references`) rather than the file-dependency graph.
Nodes are individual symbols (functions, methods, types) and edges are
caller→callee references weighted by reference kind. The resulting communities
are *behavioural* units — a feature, a subsystem, a collaborating set of
functions — that frequently cut across file and directory boundaries, which the
file-level `clusters` view cannot show.

Use `clusters` to answer "what are this repo's modules?" and `symbol-clusters`
to answer "which symbols form a feature together, regardless of where they live?".

### Subcommands

```bash
# List all symbol communities detected in the repository
codesearch symbol-clusters list my-repo

# JSON output
codesearch symbol-clusters list my-repo --format json

# Show which community a symbol belongs to. The symbol is resolved by exact
# fully-qualified name, then short-name suffix, then substring (case-sensitive).
codesearch symbol-clusters get authenticate my-repo
codesearch symbol-clusters get "MyNs/Auth#authenticate()." my-repo
```

### Options

| Flag | Subcommand | Default | Description |
|------|------------|---------|-------------|
| `-F, --format` | `list`, `get` | `text` | Output format: `text` or `json` (vimgrep is not supported) |

Requires the repository to have been indexed with call-graph support (the
default). When the call graph is empty, no communities are returned.

### Example: `symbol-clusters list`

```text
Symbol communities for `my-repo` — 5 communities, 214 symbols, 087 edges
────────────────────────────────────────────────────────────
  1. authenticate (18 symbols, rust, cohesion 0.71)
      MyNs/Auth#authenticate().
      MyNs/Auth#verify_password().
      MyNs/Session#issue_token().
      … and 15 more
  2. indexing (24 symbols, rust, cohesion 0.66)
      …
```

### Example: `symbol-clusters get`

```text
Symbol `authenticate` belongs to community `authenticate` (18 symbols, rust, cohesion 0.71)
    MyNs/Auth#authenticate().
    MyNs/Auth#verify_password().
    …
```

## Coupling Elements (`codesearch couplings`)

`couplings` answers a question the cluster listings cannot: **which single file,
symbol, or dependency is holding a detected community together?** A *coupling
element* is defined counterfactually — a node or edge whose removal would make
one Leiden community fall apart into two sub-blocks that were latent inside it
all along. That is the classic *hub-like dependency* / *modularity violation*
smell: two modules that only read as one because a single element glues them.

Rather than ablating every element and re-clustering the whole graph (which
would take hours), the command runs a **filter-then-verify** pipeline, local to
one community at a time:

1. **Localize** — each community's induced subgraph is re-clustered across a
   resolution (γ) ladder. A community that never separates has no internal
   2-block structure and is skipped; one that holds at low γ but separates at a
   higher rung is *fragile*, and the split names its latent sub-blocks A and B.
2. **Score candidates cheaply** — the minimum cut between A and B is literally
   the glue edge set; cut shares aggregated onto incident nodes plus the
   Guimerà–Amaral participation coefficient rank node candidates.
3. **Verify by ablation** — each top candidate is removed and the subgraph is
   re-clustered under several refinement seeds. Leiden is stochastic, so the
   reported `split_probability` is the *fraction* of runs in which A separates
   from B, compared against the same fraction with the element still present
   (`baseline_split_probability`). The difference is the element's
   **coupling strength**.
4. **Sweep resolution** — verified couplers are re-tested down the γ ladder, so
   the report shows the range of resolutions over which the element controls
   the merge instead of a yes/no at one arbitrary setting.

```bash
# Coupling elements in the file-dependency graph (default level)
codesearch couplings -r my-repo

# Same analysis over the symbol call graph
codesearch couplings -r my-repo --level symbol

# JSON for tooling
codesearch couplings -r my-repo --format json
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-l, --level` | `file` | Which graph: `file` (modules) or `symbol` (behavioural communities) |
| `-F, --format` | `text` | Output format: `text` or `json` |

The baseline partition is computed with the same code path as `clusters` /
`symbol-clusters`, so the community ids in the report match those commands'
output.

### Example

```text
Coupling analysis for `my-repo` (file level)
12 communities — 3 internally fragile, 1 with verified couplers
────────────────────────────────────────────────────
  1. c-4be91ca03d21 (14 files) — holds to γ≤0.2, splits at γ=0.4 into 8 + 6
     block A (8): src/orders/create.rs, src/orders/mod.rs, … 4 more
     block B (6): src/billing/invoice.rs, src/billing/mod.rs, … 2 more
     couplers:
     • node src/orders/billing_glue.rs — strength 0.88 (split probability 1.00 vs baseline 0.12), cut share 0.81, participation 0.47, active γ 0.05–0.2
     • edge src/orders/mod.rs ↔ src/billing/invoice.rs — strength 0.50 (split probability 0.62 vs baseline 0.12), cut share 0.19, active γ 0.2–0.2
```

Reading the report: the community is really an *orders* block and a *billing*
block; `billing_glue.rs` carries most of the min-cut between them and removing
it makes the community split in every seeded re-clustering. High-strength
couplers with a wide active-γ range are refactoring targets — splitting or
inverting that dependency separates the two modules cleanly.

Unlike `clusters` and `symbol-clusters`, this analysis is not cached — it is
recomputed on each invocation (the command is read-only and safe to run
concurrently with searches).

### From AI tools and the management API

The same analysis is exposed by the servers started with `codesearch serve`
(and `codesearch mcp`):

- **MCP tool** `couplings` — arguments `repository_id` (required) and `level`
  (`file` or `symbol`, default `file`); returns the `CouplingReport` as JSON.
- **REST** `GET /api/couplings?repository=<name-or-id>&level=file|symbol` —
  returns the same `CouplingReport`. `level` defaults to `file`; an unknown
  value is a `400`.

## Visualizing the Graph (`codesearch visualize`)

`visualize` renders the Leiden communities — at either level — into a shareable
file, coloured by community. It reuses the exact partition, names, and cohesion
from `clusters` / `symbol-clusters`, so the picture matches the text output.

```bash
# Interactive HTML of the file-dependency graph (default level + format)
codesearch visualize my-repo --output graph.html

# Symbol call graph as an interactive page
codesearch visualize my-repo --level symbol --output symbols.html

# Static SVG for a README, or an Obsidian canvas
codesearch visualize my-repo --format svg    --output graph.svg
codesearch visualize my-repo --format canvas --output graph.canvas

# Collapse to a one-node-per-community meta-graph (auto-applied for huge graphs)
codesearch visualize my-repo --aggregate --output overview.html
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-l, --level` | `file` | Which graph: `file` (modules) or `symbol` (behavioural communities) |
| `-F, --format` | `html` | `html`, `svg`, or `canvas` |
| `-o, --output` | `codesearch-graph.<ext>` | Path to write the artifact to |
| `--aggregate` | `false` | Render the community meta-graph instead of every node |
| `--node-limit` | `5000` | Auto-aggregate when the graph exceeds this many nodes |

### Formats

- **HTML** — a self-contained [vis-network](https://visjs.github.io/vis-network/)
  page: nodes coloured/sized by community and degree, a search box, per-community
  filter toggles, a click-to-inspect panel, and force-directed layout that
  freezes once settled. (The page loads vis-network from a CDN, so rendering it
  needs network access in the browser.)
- **SVG** — a static image laid out with a deterministic force simulation; drop
  it straight into Markdown, a README, or Notion. No JavaScript.
- **Canvas** — an Obsidian `.canvas` file; each community is a labelled group.

> Above `--node-limit` nodes, the HTML/SVG views would be an unreadable hairball,
> so `visualize` automatically switches to the aggregated community meta-graph
> (one node per community, edges weighted by cross-community link count). Pass
> `--aggregate` to force that view at any size.

## Cross-repository Usage (`codesearch uses`)

`codesearch uses <from> <to>` lists every file in the `<from>` repository that
references symbols defined in the `<to>` repository, grouped by the target file they
depend on. Both arguments accept a repository name or ID. This is useful for auditing
the surface area one service consumes from a shared library or another service.

```bash
# Files in the `web` repo that use files from the `core` repo
codesearch uses web core
```

### Example output

```text
Files in 'web' that use files from 'core':

  core/src/db.rs
    ← web/src/handlers/users.rs  [query, execute]
    ← web/src/handlers/auth.rs  [query]
  core/src/models.rs
    ← web/src/handlers/users.rs  [User, Session]

2 file(s) in 'web' depend on 2 file(s) in 'core'.
```

Each `←` line names a consuming file; the bracketed list shows the referenced symbols.
If there are no cross-repository references, the command reports that no dependencies
were found.

## Querying the Graph from AI Tools (`query_graph`)

When running as an [MCP server](./editor-integrations.md#mcp-context-server-ai-assistant-integration),
CodeSearch exposes the `query_graph` tool for precise, single-relationship queries over
the call graph. Rather than returning every edge kind at once, it returns only the
intention you ask for:

| Pattern | Returns |
|---------|---------|
| `callers_of` | Symbols that call the target |
| `callees_of` | Symbols the target calls |
| `imports_of` | What the target imports (import edges only) |
| `importers_of` | Who imports the target (import edges only) |
| `inheritors_of` | Symbols that inherit from / implement the target |
| `children_of` | Symbols the target inherits from / implements |
| `tests_for` | Test functions or files that exercise the target |
| `file_summary` | All symbols referenced within a file |

See [Editor Integrations — MCP Server](./editor-integrations.md) for tool parameters.
