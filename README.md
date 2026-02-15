# codesearch

A semantic code search tool that indexes code repositories using embeddings and AST analysis for intelligent code discovery.

## Features

- **Semantic search**: uses ML embeddings to find semantically similar code
- **AST-aware**: parses code using tree-sitter for structure-aware indexing
- **Multi-language support**: supports Rust, Python, JavaScript, TypeScript, Go
- **Persistent storage**: DuckDB with VSS (Vector Similarity Search) acceleration
- **Flexible backends**: supports ChromaDB and in-memory storage
- **Fast indexing**: efficient batch processing with ONNX embedding generation

## Architecture

This project follows Domain-Driven Design (DDD) principles:

```
crates/
├── domain/
|── application/
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

# Delete a repository by name or path
codesearch delete my-repo
codesearch delete /path/to/repo
```

### Configuration Options

| Flag | Default | Description |
|------|---------|-------------|
| `--data-dir` | `~/.codesearch` | Directory for DuckDB database files |
| `--namespace` | `main` | DuckDB schema namespace for vector storage |
| `--chroma-url` | (optional) | Use ChromaDB instead of DuckDB for vectors |
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

# Use ChromaDB for vector storage instead of DuckDB
codesearch --chroma-url http://chroma.internal:8000 index /path/to/repo --name my-repo

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

## Editor Integrations

### Neovim / Telescope

A [Telescope](https://github.com/nvim-telescope/telescope.nvim) extension is included under `ide/nvim/`. It provides a fuzzy picker over semantic search results, with file preview at the correct line.

**Setup:**

1. Add `ide/nvim/lua` to your Neovim runtime path:

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

### Storage Backends

| Mode | Persistence | Use Case |
|------|-------------|----------|
| **DuckDB** (default) | Persistent | Fast semantic search with VSS acceleration, no external dependencies |
| **ChromaDB** | Persistent | Remote vector storage, useful for distributed systems |
| **In-memory** (`--memory-storage`) | None | Testing, development, ephemeral indexing |

**Storage Details:**
- **Metadata**: Always stored in DuckDB locally via `DuckdbMetadataRepository` (repository info, chunks, file paths, statistics)
- **Vectors**: DuckDB (default) or ChromaDB (with `--chroma-url`)
- **Index**: DuckDB uses HNSW (Hierarchical Navigable Small World) for Vector Similarity Search with cosine distance

## Reranking

CodeSearch supports optional reranking to improve search result relevance using cross-encoder models.

### How It Works

1. Initial vector search retrieves candidates (minimum 100, or `num × 10` if `num > 10`)
2. A cross-encoder model (mxbai-rerank-xsmall-v1) reranks candidates based on query-document relevance
3. Top `num` reranked results are returned

### Usage

```bash
codesearch search "authentication"

# Customize number of results
codesearch search "error handling" --num 20

# Combine with filters
codesearch search "validation" --language rust --min-score 0.7
```

### Models

- **Default**: `mixedbread-ai/mxbai-rerank-xsmall-v1` (70M parameters, ONNX)
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
- [chromadb](https://www.trychroma.com/) - Alternative vector database backend
- [tokio](https://tokio.rs/) - Async runtime

## License

MIT License
