# Getting Started

## Prerequisites

- Rust 1.70 or later
- ChromaDB server (optional, for production use)

## Installation

### Build from Source

```bash
# Clone the repository
git clone https://github.com/codesearch/codesearch
cd codesearch

# Build in release mode
cargo build --release

# Copy binary to bin directory
cp target/release/codesearch bin/

# Or install system-wide
cargo install --path crates/cli
```

### Start ChromaDB (Optional)

For production use with persistent vector storage:

```bash
# Using Docker
docker run -p 8000:8000 chromadb/chroma

# Or using pip
pip install chromadb
chroma run
```

## Quick Start

### Index a Repository

```bash
# Index a local repository
codesearch index /path/to/your/project

# Index with a custom name
codesearch index /path/to/your/project --name "My Project"

# Use in-memory storage (for testing)
codesearch --in-memory index /path/to/your/project
```

### Search for Code

```bash
# Basic search
codesearch search "function that handles user authentication"

# Search with more results
codesearch search "error handling" --limit 20

# Filter by language
codesearch search "parse json" --language rust

# Filter by minimum score
codesearch search "database connection" --min-score 0.5
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

## Supported Languages

- Rust (`.rs`)
- Python (`.py`)
- JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`)
- TypeScript (`.ts`, `.tsx`)
- Go (`.go`)

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
