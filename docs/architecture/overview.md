# Architecture Overview

CodeSearch follows Domain-Driven Design (DDD) principles with a clean separation of concerns.

## Layer Structure

```
┌─────────────────────────────────────────────────────────┐
│                         CLI                              │
│              (User Interface Layer)                      │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                    Application                           │
│               (Use Cases / Orchestration)                │
│                                                         │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────────┐   │
│  │    Index    │ │   Search    │ │     Delete      │   │
│  │  Repository │ │    Code     │ │   Repository    │   │
│  └─────────────┘ └─────────────┘ └─────────────────┘   │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                      Domain                              │
│            (Business Logic & Models)                     │
│                                                         │
│  ┌──────────┐  ┌──────────────┐  ┌─────────────────┐   │
│  │  Models  │  │  Repository  │  │    Services     │   │
│  │          │  │   Traits     │  │    (Traits)     │   │
│  └──────────┘  └──────────────┘  └─────────────────┘   │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                     Connector                            │
│            (External Integrations)                       │
│                                                         │
│  ┌──────────┐  ┌──────────────┐  ┌─────────────────┐   │
│  │ Embedding│  │   Storage    │  │     Parser      │   │
│  │(FastEmbed)│ │(SQLite/Chroma)│ │  (Tree-sitter)  │   │
│  └──────────┘  └──────────────┘  └─────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

## Layers

### CLI Layer (`crates/cli`)

The command-line interface that users interact with. Responsible for:
- Parsing command-line arguments
- Initializing dependencies
- Formatting and displaying output

### Application Layer (`crates/application`)

Contains use cases that orchestrate business operations:
- **IndexRepositoryUseCase**: Indexes a code repository
- **SearchCodeUseCase**: Performs semantic search
- **ListRepositoriesUseCase**: Lists indexed repositories
- **DeleteRepositoryUseCase**: Removes a repository from the index

### Domain Layer (`crates/domain`)

The core of the application containing:
- **Models**: `CodeChunk`, `Repository`, `Embedding`, `SearchResult`, etc.
- **Repository Traits**: Interfaces for data persistence
- **Service Traits**: Interfaces for business operations

### Connector Layer (`crates/connector`)

Implements external integrations:
- **Embedding**: FastEmbed for vector generation
- **Storage**: SQLite for metadata, ChromaDB for vectors
- **Parser**: Tree-sitter for AST parsing

## Data Flow

### Indexing Flow

```
Repository Path
      │
      ▼
┌─────────────┐
│  Walk Files │
└─────────────┘
      │
      ▼
┌─────────────┐
│Parse w/ TS  │ ─── Extract functions, classes, etc.
└─────────────┘
      │
      ▼
┌─────────────┐
│  Generate   │ ─── FastEmbed (all-MiniLM-L6-v2)
│  Embeddings │
└─────────────┘
      │
      ├──────────────────┐
      ▼                  ▼
┌─────────────┐    ┌─────────────┐
│   SQLite    │    │  ChromaDB   │
│ (metadata)  │    │ (vectors)   │
└─────────────┘    └─────────────┘
```

### Search Flow

```
Query String
      │
      ▼
┌─────────────┐
│   Embed     │ ─── FastEmbed
│   Query     │
└─────────────┘
      │
      ▼
┌─────────────┐
│  Vector     │ ─── ChromaDB similarity search
│  Search     │
└─────────────┘
      │
      ▼
┌─────────────┐
│   Fetch     │ ─── SQLite (full chunk data)
│  Metadata   │
└─────────────┘
      │
      ▼
Search Results
```

## Design Decisions

### Why DDD?

- Clear separation of concerns
- Domain logic is isolated and testable
- Easy to swap implementations (e.g., different vector databases)
- Follows the dependency inversion principle

### Why Tree-sitter?

- Fast, incremental parsing
- Multi-language support
- Produces concrete syntax trees
- Battle-tested in many editors

### Why FastEmbed?

- Rust-native embedding generation
- Uses ONNX Runtime for inference
- Supports multiple embedding models
- No Python dependency required

### Why ChromaDB?

- Purpose-built for embeddings
- Simple HTTP API
- Supports persistence
- Easy to deploy
