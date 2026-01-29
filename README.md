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

```bash
# Index a repository
codesearch index /path/to/repo

# Search for code
codesearch search "function that handles authentication"

# Show indexed repositories
codesearch list
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

- [ort](https://github.com/pykeio/ort) - ONNX Runtime for ML inference
- [fastembed-rs](https://github.com/Anush008/fastembed-rs) - Fast embedding generation
- [tree-sitter](https://tree-sitter.github.io/) - AST parsing
- [ChromaDB](https://www.trychroma.com/) - Vector database for embeddings
- [SQLite](https://www.sqlite.org/) - Local database for metadata

## License

MIT License
