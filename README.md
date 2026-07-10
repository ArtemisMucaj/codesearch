# codesearch

A semantic code search tool that indexes code repositories using embeddings and AST analysis for intelligent code discovery.

## Features

- **Hybrid search** (default): combines semantic vector search with BM25-style keyword matching, fused via Reciprocal Rank Fusion (RRF) for best-of-both precision and recall
- **Semantic search**: uses ML embeddings to find conceptually similar code even without exact keyword matches
- **AST-aware**: parses code using tree-sitter for structure-aware indexing
- **Multi-language support**: supports Rust, Python, JavaScript, TypeScript, Go, HCL, PHP, C++
- **Persistent storage**: DuckDB with VSS (Vector Similarity Search) acceleration
- **Fast indexing**: efficient batch processing with ONNX embedding generation

## Architecture

This project follows Domain-Driven Design (DDD) principles:

```
src/
├── domain/
├── application/
├── connector/
└── cli/
```

## Installation

```bash
cargo build --release
# Binary will be placed in bin/
cp target/release/codesearch bin/
```

## Usage

### Getting Started

No external services required! CodeSearch uses DuckDB by default for persistent storage.

```bash
# Build the project
cargo build --release

# Index a repository
./target/release/codesearch index /path/to/repo --name my-repo

# Search
./target/release/codesearch search "function that handles authentication"

# List indexed repositories
./target/release/codesearch list
```

### Commands

```bash
codesearch index /path/to/repo

# Search for code
codesearch search "function that handles authentication"

# Show indexed repositories
codesearch list

# Show indexing statistics
codesearch stats

# Delete a repository by name or path
codesearch delete my-repo
codesearch delete /path/to/repo

# Show the blast radius of a symbol change (BFS over call graph)
codesearch impact authenticate

# Show full caller/callee call-chain context for a symbol
codesearch context authenticate

# LLM-powered explanation of a symbol's full call flow and business purpose
codesearch explain authenticate

# Rank entry-point execution features by criticality
codesearch features list my-repo

# Detect architectural clusters in the file dependency graph
codesearch clusters list my-repo

# Render the communities as an interactive HTML graph (or svg/canvas)
codesearch visualize my-repo --output graph.html

# List the files one repository uses from another
codesearch uses web core

# Launch the interactive TUI (search, impact, and context in one terminal UI)
codesearch tui

# Start MCP server (stdio, for AI tool integration)
codesearch mcp

# Start MCP server over HTTP
codesearch mcp --http 8080
```

### Configuration Options

| Flag | Default | Description |
|------|---------|-------------|
| `--data-dir` | `~/.codesearch` | Directory for DuckDB database files |
| `--namespace` | `search` | DuckDB schema namespace for vector storage |
| `--memory-storage` | `false` | Use in-memory storage (no persistence) |
| `--mock-embeddings` | `false` | Use mock embeddings (for testing) |
| `--no-rerank` | `false` | Disable reranking|
| `-v, --verbose` | `false` | Enable debug logging |

### Search Options

| Flag | Default | Description |
|------|---------|-------------|
| `--num` | `10` | Number of results to return |
| `-m, --min-score` | (none) | Minimum relevance score threshold (0.0-1.0) |
| `-L, --language` | (none) | Filter by programming language (can specify multiple) |
| `-r, --repository` | (none) | Filter by repository (can specify multiple) |
| `-F, --format` | `text` | Output format: `text`, `json`, or `vimgrep` |
| `--no-text-search` | (off) | Disable the keyword leg; use only vector/semantic search |

### Output Formats

| Format | Description |
|--------|-------------|
| `text` | Human-readable output with code previews (default) |
| `json` | Structured JSON array for programmatic use and editor integrations |
| `vimgrep` | `file:line:col:text` format for Neovim quickfix list and Telescope |

### Examples

```bash
# Index with a custom data directory
codesearch --data-dir /var/lib/codesearch index /path/to/repo --name my-repo

# Use a separate namespace for different projects
codesearch --namespace project-a index /path/to/repo-a --name repo-a
codesearch --namespace project-b index /path/to/repo-b --name repo-b

# Verbose logging with debug output
codesearch -v search "authentication error handling"

# Use mock embeddings for testing
codesearch --mock-embeddings index ./test-repo --name test

codesearch search "error handling" --num 25

# Filter by language
codesearch search "async function" --language rust

# JSON output for scripts or editor integrations
codesearch search "error handling" --format json

# Vimgrep format for Neovim quickfix
codesearch search "error handling" --format vimgrep
```

## Call Graph Analysis

CodeSearch builds a call graph during indexing and exposes two commands to query it: **`impact`** for blast-radius analysis and **`context`** for 360-degree dependency views.

### Impact Analysis

Shows every symbol that would be affected (transitively) if a given symbol changes. Uses BFS over the call graph, grouping affected symbols by hop depth.

```bash
# Show what breaks if `authenticate` changes
codesearch impact authenticate

# Restrict to a specific repository
codesearch impact authenticate --repository my-api

# JSON output (for scripts)
codesearch impact authenticate --format json

# Vimgrep output for Neovim quickfix
codesearch impact authenticate --format vimgrep
```

**Example output:**
```
Impact analysis for 'authenticate'
─────────────────────────────────────────
process_request [call]  src/router.rs:10
└── handle_login [call]  src/api/auth.rs:42
    └── authenticate

run_tests [call]  tests/integration.rs:5
└── verify_token [call]  src/middleware/auth.rs:18
    └── authenticate
```

### Symbol Context

Shows the 360-degree dependency view for a symbol: who calls it (callers) and what it calls (callees).

```bash
# Show callers and callees of `authenticate`
codesearch context authenticate

# Restrict to a specific repository
codesearch context authenticate --repository my-api

# JSON output
codesearch context authenticate --format json

# Vimgrep output for Neovim quickfix
codesearch context authenticate --format vimgrep
```

**Example output** (caller chains as trees, callees hanging off the queried symbol):
```
Context for 'authenticate'
─────────────────────────────────────────
process_request [call]  src/router.rs:10
└── handle_login [call]  src/api/auth.rs:42
    └── authenticate
        ├── hash_password [call]  src/crypto/hash.rs:10
        └── lookup_user [call]  src/db/users.rs:55
```

### Call Graph Options

| Flag | Command | Default | Description |
|------|---------|---------|-------------|
| `-r, --repository` | both | (none) | Restrict to a specific repository |
| `-F, --format` | both | `text` | Output format: `text`, `json`, or `vimgrep` |
| `--regex` | both | off | Treat the symbol as an explicit POSIX regex (no auto-wrapping) |

> **Symbol matching:** By default the symbol argument is matched as a substring
> (`load` matches any fully-qualified name containing `load`). Pass `--regex` to
> supply your own anchored pattern, e.g. `codesearch impact "^MyNs/.*Service#get$" --regex`.

> **Note:** Call graph data is populated during `codesearch index`. Re-index after code changes to keep the graph up to date.

## LLM Explanation (`explain`)

Uses an LLM to explain a symbol's complete call flow, data flow, and business purpose — requires `ANTHROPIC_API_KEY` (default) or an OpenAI-compatible endpoint.

```bash
codesearch explain authenticate
codesearch explain authenticate --llm open-ai
```

See [Call Graph Analysis — LLM Explanation](docs/features/call-graph.md#llm-explanation-codesearch-explain) for the full flag reference, environment variables, and example output.

## Long-Term Memory (`memory`)

Import finished assistant sessions (Claude Code transcripts or generic JSONL
chat logs) and distill them into durable, searchable memories — user
preferences, reusable experiences, procedural skills, and project facts.
Extraction uses a small LLM via the same provider configuration as `explain`;
memories live in their own database (`~/.codesearch/memory.duckdb`), separate
from the code index.

```bash
codesearch memory import ~/.claude/projects/<project>/<session-id>.jsonl
codesearch memory search "how do we handle lock conflicts"
codesearch memory list --kind preference

# Browse the memory virtual filesystem (OpenViking-style L0/L1 abstracts)
codesearch memory tree                     # roots: the rollup + stored sessions
codesearch memory show memory://memory     # the "read this first" summary
codesearch memory show memory://sessions/<id>   # one session's transcript
```

Each import also stores the session as a node in a `memory://` virtual
filesystem (with a generated L0 abstract, L1 overview, and its full transcript)
and regenerates a whole-memory rollup at `memory://memory` — a summary an agent
reads first before drilling into individual memories.

See [Long-Term Memory](docs/features/memory.md) for the memory kinds, the
virtual filesystem, update semantics, and model configuration.

## Interactive TUI (`tui`)

A full-screen terminal UI combining search, impact analysis, and context lookup in one interface.

```bash
codesearch tui
codesearch tui --mode impact
codesearch tui --query "authentication"
```

See [Getting Started — Launch the Interactive TUI](docs/features/getting-started.md#launch-the-interactive-tui) for all options.

## Architecture & Dependency Analysis

Beyond per-symbol call graphs, CodeSearch analyses the file- and repository-level
dependency graph built during indexing.

### Execution Features (`features`)

Discovers entry-point execution flows (forward call chains rooted at entry-point
symbols) and ranks them by a criticality score.

```bash
# List the most critical features in a repository
codesearch features list my-repo --limit 20

# Show a single feature by its entry-point symbol
codesearch features get handle_request

# Show which features are impacted by changing one or more symbols
codesearch features impacted authenticate hash_password
```

### Clusters (`clusters`)

Runs the [Leiden](https://en.wikipedia.org/wiki/Leiden_algorithm) community-detection
algorithm over the file-level call graph to surface tightly-coupled groups of files
(architectural modules).

```bash
# List detected clusters
codesearch clusters list my-repo

# Find which cluster a file belongs to
codesearch clusters get src/api/auth.rs my-repo

# Print a high-level Markdown architecture overview table
codesearch clusters overview my-repo
```

### Symbol Clusters (`symbol-clusters`)

Runs the same Leiden algorithm over the **symbol** call graph instead of files,
grouping individual functions, methods, and types into behavioural communities
that often cut across file boundaries.

```bash
# List detected symbol communities
codesearch symbol-clusters list my-repo

# Find which community a symbol belongs to (exact, short-name, or substring)
codesearch symbol-clusters get authenticate my-repo

# JSON output (vimgrep is not supported for either subcommand)
codesearch symbol-clusters list my-repo --format json
```

### Visualize (`visualize`)

Renders the Leiden communities — file-level or symbol-level — to a shareable
file, coloured by community. Formats: an interactive **HTML** graph
(vis-network; search, per-community filters, click-to-inspect), a static
**SVG**, or an Obsidian **canvas**.

```bash
# Interactive HTML of the file-dependency graph (defaults)
codesearch visualize my-repo --output graph.html

# Symbol call graph instead of files
codesearch visualize my-repo --level symbol --output symbols.html

# Other formats
codesearch visualize my-repo --format svg    --output graph.svg
codesearch visualize my-repo --format canvas --output graph.canvas

# One node per community (auto-applied above --node-limit, default 5000)
codesearch visualize my-repo --aggregate --output overview.html
```

See [Architecture & Dependency Analysis](docs/features/architecture-analysis.md#visualizing-the-graph-codesearch-visualize)
for the full option reference.

### Cross-repository Usage (`uses`)

Lists every file in one repository that references symbols defined in another,
grouped by the target file they depend on.

```bash
# Files in the `web` repo that use files from the `core` repo
codesearch uses web core
```

See [Architecture & Dependency Analysis](docs/features/architecture-analysis.md) for
output examples, flags, and JSON schemas.

## Editor Integrations

### Neovim / Telescope

A [Telescope](https://github.com/nvim-telescope/telescope.nvim) extension is included under `ide/nvim/`. It provides a fuzzy picker over semantic search results, with file preview at the correct line.

**Setup:**

1. Add `ide/nvim` to your Neovim runtime path (Neovim resolves the `lua/` subdirectory automatically):

```lua
vim.opt.runtimepath:append("/path/to/codesearch/ide/nvim")
```

2. Load the extension:

```lua
require("telescope").load_extension("codesearch")
```

3. Bind a key:

```lua
vim.keymap.set("n", "<leader>cs", function()
  require("telescope").extensions.codesearch.codesearch()
end, { desc = "Semantic code search" })
```

**Configuration (optional):**

```lua
require("telescope").setup({
  extensions = {
    codesearch = {
      bin = "codesearch",     -- path to binary
      num = 20,               -- number of results
      min_score = 0.3,        -- minimum relevance score
      data_dir = nil,         -- custom data directory
      namespace = nil,        -- custom namespace
    },
  },
})
```

**Quick use without Telescope:**

```bash
# Load results directly into Neovim's quickfix list
codesearch search "error handling" --format vimgrep | nvim -q /dev/stdin
```

### MCP Server

CodeSearch can run as a [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server, allowing AI tools (Claude, Cursor, etc.) to search your codebase semantically.

**Stdio mode** (default, for local AI tool integration):

```bash
codesearch mcp
```

**HTTP mode** (for network-accessible deployments):

```bash
# Listen on localhost:8080
codesearch mcp --http 8080

# Listen on all interfaces (public)
codesearch mcp --http 8080 --public
```

The HTTP server exposes the MCP endpoint at `/mcp`.

**Exposed tools:**

| Tool | Description |
|------|-------------|
| `search_code` | Hybrid/semantic search. Accepts `query`, `limit`, `min_score`, `languages`, `repositories`, and `text_search`. |
| `analyze_impact` | Blast-radius analysis for a symbol. Accepts `symbol`, `repository_id`, and `regex`. |
| `get_symbol_context` | 360° caller/callee context for a symbol. Accepts `symbol`, `repository_id`, and `regex`. |
| `query_graph` | Precise relationship queries over the call graph. Accepts `pattern`, `target`, `repository_id`, and `limit`. |
| `list_repositories` | List indexed repositories with file/chunk counts and language breakdown (also serves as stats). Takes no arguments. |
| `list_features` | Entry-point execution features scored by criticality. Accepts `repository_id` and `limit`. |
| `get_feature` | A single execution feature by entry-point symbol. Accepts `symbol` and `repository_id`. |
| `get_impacted_features` | Features whose call chain includes any changed symbol. Accepts `symbols` and `repository_id`. |
| `file_uses` | Files in one repository that depend on files in another. Accepts `from` and `to` (repository name or ID). |
| `list_clusters` | Architectural clusters via Leiden community detection. Accepts `repository_id`. |
| `get_file_cluster` | The cluster a given file belongs to. Accepts `file_path` and `repository_id`. |
| `architecture_overview` | Markdown table summarising clusters and inter-cluster dependencies. Accepts `repository_id`. |
| `list_symbol_clusters` | Symbol-level communities via Leiden over the call graph. Accepts `repository_id`. |
| `get_symbol_cluster` | The symbol community a given symbol belongs to. Accepts `symbol` and `repository_id`. |
| `search_memory` | Recall long-term memories (preferences, experiences, skills, facts) extracted from imported sessions. Accepts `query`, `kind`, and `limit`. |
| `list_memories` | List stored memories, newest first. Accepts `kind`. |
| `read_memory` | Read the memory virtual filesystem. Call with no args first for the whole-memory rollup, then drill into `memory://` nodes (sessions, resources). Accepts `uri`. |

The `query_graph` tool supports eight intention-named relationship `pattern`s, returning
only the requested edge type instead of every relationship at once:

`callers_of`, `callees_of`, `imports_of`, `importers_of`, `inheritors_of`,
`children_of`, `tests_for`, and `file_summary`.

### Storage Backends

| Mode | Persistence | Use Case |
|------|-------------|----------|
| **DuckDB** (default) | Persistent | Fast semantic search with VSS acceleration, no external dependencies |
| **In-memory** (`--memory-storage`) | None | Testing, development, ephemeral indexing |

**Storage Details:**
- **Metadata**: Always stored in DuckDB locally via `DuckdbMetadataRepository` (repository info, chunks, file paths, statistics)
- **Vectors**: DuckDB with HNSW (Hierarchical Navigable Small World) for Vector Similarity Search with cosine distance

## Hybrid Search

By default, every `search` query runs two complementary retrieval legs and fuses them with Reciprocal Rank Fusion (RRF):

1. **Semantic leg** — vector similarity via HNSW cosine distance (finds conceptually related code)
2. **Keyword leg** — BM25-style LIKE matching on content and symbol names (finds exact keyword occurrences)

RRF assigns each result a score of `1 / (60 + rank)` from each leg it appears in; items found by both legs accumulate the highest fused scores. Final scores are in the ~0.016–0.033 range.

```bash
# Hybrid search (default — no flag needed)
codesearch search "parse configuration file"

# Semantic-only (disable keyword leg)
codesearch search "parse configuration file" --no-text-search
```

## Reranking

CodeSearch supports optional reranking to improve search result relevance using cross-encoder models.

### How It Works

1. Initial hybrid/vector search retrieves candidates using inverse-log scaling: `num + ⌈num / ln(num)⌉` (defaults to 20 base candidates when `num ≤ 10`)
2. For semantic-only results, candidates with vector similarity score below 0.1 are excluded (too irrelevant to benefit from reranking); hybrid RRF results bypass this filter because RRF scores are intentionally small (~0.016–0.033)
3. A cross-encoder model (bge-reranker-base) reranks remaining candidates based on query-document relevance
4. Top `num` reranked results are returned

### Usage

```bash
codesearch search "authentication"

# Customize number of results
codesearch search "error handling" --num 20

# Combine with filters
codesearch search "validation" --language rust --min-score 0.7
```

### Models

- **Default**: `BAAI/bge-reranker-base` (110M parameters, ONNX)
- Downloaded automatically from HuggingFace Hub on first use
- No API key or external service required

## Logging

CodeSearch uses structured logging with sensible defaults to keep output clean while providing detailed information when needed.

### Default Behavior

By default, only application-level logs are shown:
- Indexing progress and completion
- Search queries and results
- Reranking operations
- Repository deletion

Logs from external dependencies (ONNX runtime, tokenizers, database drivers, etc.) are suppressed to reduce noise.

### Verbose Mode

Use `-v` or `--verbose` to enable debug-level logging for troubleshooting:

```bash
codesearch -v index /path/to/repo
codesearch -v search "authentication"
```

This shows additional details like:
- File processing progress
- Model initialization
- Storage backend configuration

### Advanced: External Crate Logs

To debug issues with external dependencies, use the `RUST_LOG` environment variable:

```bash
# Debug ONNX runtime issues
RUST_LOG=warn,codesearch=info,ort=debug codesearch index /path/to/repo

# Debug database issues
RUST_LOG=warn,codesearch=info,duckdb=debug codesearch search "query"

# Debug everything (very verbose)
RUST_LOG=debug codesearch index /path/to/repo
```

## Development

```bash
# Run tests
cargo test

# Run with logging
RUST_LOG=debug cargo run -- index /path/to/repo

# Format code
cargo fmt

# Run linter
cargo clippy
```

## Dependencies

- [ort](https://github.com/pykeio/ort) - ONNX Runtime for ML embedding inference
- [tree-sitter](https://tree-sitter.github.io/) - AST parsing and code extraction
- [duckdb-rs](https://github.com/duckdb/duckdb-rs) - DuckDB Rust bindings with VSS extension
- [tokio](https://tokio.rs/) - Async runtime

## License

MIT License
