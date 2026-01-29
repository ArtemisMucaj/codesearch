# codesearch

A semantic code search tool that indexes code repositories using embeddings and AST analysis for intelligent code discovery.

## Features

- **Semantic search**: uses ML embeddings to find semantically similar code
- **AST-aware**: parses code using tree-sitter for structure-aware indexing
- **Multi-language support**: supports Rust, Python, JavaScript, TypeScript, Go
- **Persistent storage**: ChromaDB for embeddings, SQLite for AST metadata
- **Fast indexing**: efficient incremental indexing of large codebases

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

### ChromaDB

Chroma vector store is required for codesearch to operate.

```bash
# Start ChromaDB server
docker run -d -p 8000:8000 chromadb/chroma

# Verify it's running
curl http://localhost:8000/api/v1/heartbeat
```

Default endpoint is localhost:8000

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

### Configuration options

| Flag | Default | Description |
|------|---------|-------------|
| `--chroma-url` | `http://localhost:8000` | ChromaDB server URL |
| `--chroma-collection` | `codesearch` | ChromaDB collection name |
| `--memory-storage` | `false` | Use in-memory storage (embeddings lost on exit) |
| `--data-dir` | `~/.codesearch` | Directory for SQLite metadata |
| `--mock-embeddings` | `false` | Use mock embeddings (for testing) |
| `--model` | (default model) | Custom embedding model path |
| `-v, --verbose` | `false` | Enable debug logging |

### Examples

```bash
# Use a custom ChromaDB instance
codesearch --chroma-url http://chroma.internal:8000 index /path/to/repo

# Use a separate collection for a project
codesearch --chroma-collection my-project search "error handling"

# Verbose logging
codesearch -v search "authentication"
```

### Vector store

| Mode | Persistence | Use Case |
|------|-------------|----------|
| **ChromaDB** (default) | Persistent | Retains embeddings across sessions |
| **In-memory** (`--memory-storage`) | None | Testing |

If ChromaDB is not available, codesearch will automatically fall back to in-memory storage with a warning.

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

- [ort](https://github.com/pykeio/ort) - ONNX Runtime for ML inference
- [fastembed-rs](https://github.com/Anush008/fastembed-rs) - Fast embedding generation
- [tree-sitter](https://tree-sitter.github.io/) - AST parsing
- [ChromaDB](https://www.trychroma.com/) - Vector database for embeddings
- [SQLite](https://www.sqlite.org/) - Local database for metadata

## License

MIT License
