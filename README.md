# codesearch

A semantic code search tool that indexes code repositories using embeddings and AST analysis for intelligent code discovery.

## Features

- **Semantic Search**: Uses ML embeddings to find semantically similar code
- **AST-Aware**: Parses code using tree-sitter for structure-aware indexing
- **Multi-Language Support**: Supports Rust, Python, JavaScript, TypeScript, Go
- **Persistent Storage**: ChromaDB for embeddings, SQLite for AST metadata
- **Fast Indexing**: Efficient incremental indexing of large codebases

## Architecture

This project follows Domain-Driven Design (DDD) principles:

```
crates/
├── domain/        # Core business logic, models, and repository traits
├── application/   # Use cases and orchestration layer
├── connector/     # External integrations (ChromaDB, SQLite, embeddings)
└── cli/           # Command-line interface
```

## Installation

```bash
cargo build --release
# Binary will be placed in bin/
cp target/release/codesearch bin/
```

## Usage

### Prerequisites: ChromaDB

**Important**: By default, codesearch uses an in-memory vector store which means **embeddings are lost when the CLI exits**. For persistent storage, you must run ChromaDB.

#### Start ChromaDB with Docker

```bash
# Start ChromaDB server
docker run -d -p 8000:8000 chromadb/chroma

# Verify it's running
curl http://localhost:8000/api/v1/heartbeat
```

#### Alternative: Install ChromaDB locally

```bash
pip install chromadb
chroma run --host localhost --port 8000
```

### Basic Commands

```bash
# Index a repository (uses ChromaDB at localhost:8000 by default)
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

# Use in-memory storage (not recommended for production)
codesearch --memory-storage index /path/to/repo

# Enable verbose logging
codesearch -v search "authentication"
```

### Storage Modes

| Mode | Persistence | Use Case |
|------|-------------|----------|
| **ChromaDB** (default) | Persistent | Production use, retains embeddings across sessions |
| **In-memory** (`--memory-storage`) | None | Testing, one-off searches |

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
>>>>>>> a67fae0 (Add semantic code search tool with DDD architecture)
