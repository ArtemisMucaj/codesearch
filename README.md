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
| `--model` | `all-MiniLM-L6-v2` | Embedding model (from HuggingFace) |
| `-v, --verbose` | `false` | Enable debug logging |

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
