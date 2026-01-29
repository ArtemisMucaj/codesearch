# Indexing Pipeline

## Overview

The indexing pipeline transforms source code into searchable embeddings through the following stages:

1. **Repository Discovery** - Walk the file tree
2. **File Filtering** - Select supported files
3. **AST Parsing** - Extract code chunks
4. **Embedding Generation** - Create vector representations
5. **Persistence** - Store in databases

## Stage Details

### 1. Repository Discovery

Uses the `ignore` crate to walk the repository while respecting:
- `.gitignore` patterns
- Global gitignore
- `.git/info/exclude`

### 2. File Filtering

Files are filtered by:
- Extension (only supported languages)
- Binary detection
- Size limits (configurable)

### 3. AST Parsing

Tree-sitter parses each file and extracts semantic chunks:

```text
Source File
     │
     ▼
┌────────────────┐
│  Tree-sitter   │
│     Parser     │
└────────────────┘
     │
     ▼
┌────────────────┐
│  Query Match   │ ─── Language-specific queries
└────────────────┘
     │
     ▼
┌────────────────┐
│ Extract Chunks │
└────────────────┘
```

#### Extracted Node Types

| Language   | Node Types                                    |
|------------|-----------------------------------------------|
| Rust       | function, struct, enum, trait, impl, module   |
| Python     | function, class                               |
| JavaScript | function, class, method, arrow_function       |
| TypeScript | function, class, method, arrow_function       |
| Go         | function, method, type                        |

### 4. Embedding Generation

Each chunk is converted to a vector embedding:

```rust
// Chunk preparation
let text = format!("{} [{}] {}",
    symbol_name,  // e.g., "calculate_sum"
    node_type,    // e.g., "function"
    content       // The actual code
);

// Generate embedding
let embedding = model.embed(text);  // 384 dimensions
```

#### Embedding Models

| Model               | Dimensions | Sequence Length |
|---------------------|------------|-----------------|
| all-MiniLM-L6-v2    | 384        | 256             |
| bge-small-en-v1.5   | 384        | 512             |
| bge-base-en-v1.5    | 768        | 512             |

### 5. Persistence

Data is stored in two locations:

#### SQLite (Metadata)
- Repository information
- Code chunks with full content
- File paths, line numbers
- Language and node type

#### ChromaDB (Vectors)
- Embedding vectors
- Chunk IDs for lookup

## Performance Considerations

### Batch Processing

Embeddings are generated in batches for efficiency:

```rust
// Process files in chunks of 100
for batch in chunks.chunks(100) {
    let embeddings = embedding_service.embed_chunks(batch).await?;
    embedding_repo.save_batch(&embeddings).await?;
}
```

### Incremental Indexing

Future improvement: Track file hashes to avoid re-indexing unchanged files.

## Configuration Options

```rust
pub struct IndexConfig {
    /// Maximum file size to index (bytes)
    pub max_file_size: usize,

    /// Batch size for embedding generation
    pub batch_size: usize,

    /// Minimum chunk size (characters)
    pub min_chunk_size: usize,

    /// Maximum chunk size (characters)
    pub max_chunk_size: usize,
}
```

## Error Handling

The indexing pipeline handles errors gracefully:

- **Parse errors**: Logged and skipped
- **Embedding errors**: Logged and skipped
- **I/O errors**: Logged and skipped
- **Storage errors**: Propagated (fatal)

This ensures partial indexing succeeds even when some files fail.
