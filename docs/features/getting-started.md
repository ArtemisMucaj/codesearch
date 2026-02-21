# Getting Started

## Prerequisites

- Rust 1.70 or later
- No external services required (DuckDB is bundled)
- Optional: ChromaDB server (if you prefer remote vector storage)

## Installation

### Build from Source

```bash
# Clone the repository
git clone https://github.com/ArtemisMucaj/codesearch
cd codesearch

# Build in release mode
cargo build --release

# Copy binary to bin directory
cp target/release/codesearch bin/

# Or install system-wide
cargo install --path .
```

### Storage Configuration (Optional)

By default, CodeSearch uses DuckDB for both metadata and vectors. Optionally, you can use ChromaDB for remote vector storage:

```bash
# Use ChromaDB for vectors (metadata stays in DuckDB)
# First start ChromaDB
docker run -d -p 8000:8000 chromadb/chroma

# Then use it with codesearch
codesearch --chroma-url http://localhost:8000 index /path/to/repo
```

## Quick Start

### Index a Repository

```bash
# Index a local repository
codesearch index /path/to/your/project

# Index with a custom name
codesearch index /path/to/your/project --name "My Project"

# Use in-memory storage (for testing, no persistence)
codesearch --memory-storage index /path/to/your/project
```

### Search for Code

```bash
# Basic search
codesearch search "function that handles user authentication"

# Search with more results
codesearch search "error handling" --num 20

# Filter by language
codesearch search "parse json" --language rust

# Filter by minimum score
codesearch search "database connection" --min-score 0.5

# Output as JSON (for scripts and editor integrations)
codesearch search "error handling" --format json

# Output in vimgrep format (for Neovim quickfix / Telescope)
codesearch search "error handling" --format vimgrep
```

### List Indexed Repositories

```bash
codesearch list
```

### View Statistics

```bash
codesearch stats
```

### Delete a Repository

```bash
# Delete by ID
codesearch delete abc123

# Delete by path
codesearch delete /path/to/your/project
```

### Start the MCP Server

Run CodeSearch as a [Model Context Protocol](https://modelcontextprotocol.io/) server for AI tool integration:

```bash
# Stdio mode (default, for Claude Desktop / Cursor / etc.)
codesearch mcp

# HTTP mode on a specific port
codesearch mcp --http 8080

# HTTP mode accessible on all interfaces
codesearch mcp --http 8080 --public
```

## Configuration

### Data Directory

By default, CodeSearch stores data in `~/.codesearch`. You can change this:

```bash
codesearch --data-dir /custom/path index /path/to/repo
```

### Verbose Logging

Enable debug logging:

```bash
codesearch -v search "my query"
```

## How Search Works

Codesearch uses **semantic vector search**:

1. Your query is converted to a 384-dimensional embedding
2. The DuckDB VSS extension finds semantically similar code using HNSW indexes
3. A cross-encoder reranker (mxbai-rerank-xsmall-v1) rescores candidates for higher relevance (enabled by default, disable with `--no-rerank`)
4. Results are ranked by cosine similarity (0.0 to 1.0) or reranking score
5. Filters can be applied by language, node type, repository, or minimum score

**Why VSS (Vector Similarity Search)?**
- ✓ Finds conceptually similar code, not just keyword matches
- ✓ HNSW index provides fast approximate nearest neighbor search
- ✓ Built-in to DuckDB - no external service needed
- ✓ Cosine distance optimized for high-dimensional embeddings

## Supported Languages

- Rust (`.rs`)
- Python (`.py`)
- JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`)
- TypeScript (`.ts`, `.tsx`)
- Go (`.go`)
- HCL (`.hcl`, `.tf`)
- PHP (`.php`)
- C++ (`.cpp`, `.cc`, `.cxx`, `.h`, `.hpp`)

## What Gets Indexed

CodeSearch extracts and indexes the following code structures:

- Functions and methods
- Classes and structs
- Enums and traits
- Implementations
- Modules
- Constants and type definitions

Each chunk includes:
- The code content
- File path and line numbers
- Symbol name (if applicable)
- Language and node type
