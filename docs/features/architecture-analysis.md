# Architecture & Dependency Analysis

In addition to per-symbol call graph queries (`impact`, `context`, `explain` — see
[Call Graph Analysis](./call-graph.md)), CodeSearch analyses the **file- and
repository-level** dependency graph built during indexing. These commands answer
"how is this codebase structured?" rather than "where is X?".

All three commands derive from the same `SymbolReference` edges populated during
`codesearch index`, so re-index after code changes to keep the analysis current.

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
