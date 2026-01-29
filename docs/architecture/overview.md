# Architecture Overview

CodeSearch follows Domain-Driven Design (DDD) principles with a clean separation of concerns.

## Layer Structure

```text
┌─────────────────────────────────────────────────────────┐
│                         CLI                              │
│              (User Interface Layer)                      │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                    Application                           │
│             (Use Cases + Interfaces/Ports)              │
│                                                         │
│  ┌─────────────────────────────────────────────────┐   │
│  │               Use Cases                          │   │
│  │  Index | Search | List | Delete Repository      │   │
│  └─────────────────────────────────────────────────┘   │
│  ┌─────────────────────────────────────────────────┐   │
│  │            Interfaces (Ports)                    │   │
│  │  VectorRepository | RepositoryRepository |       │   │
│  │  EmbeddingService | ParserService                │   │
│  └─────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                      Domain                              │
│          (Pure Domain Models with Behavior)              │
│                                                         │
│  ┌──────────┐  ┌──────────────┐  ┌─────────────────┐   │
│  │CodeChunk │  │  Repository  │  │   Embedding     │   │
│  │          │  │              │  │                 │   │
│  └──────────┘  └──────────────┘  └─────────────────┘   │
│  ┌──────────┐  ┌──────────────┐  ┌─────────────────┐   │
│  │ Language │  │ SearchResult │  │  DomainError    │   │
│  │          │  │ /SearchQuery │  │                 │   │
│  └──────────┘  └──────────────┘  └─────────────────┘   │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                     Connector                            │
│              (Adapters / Implementations)                │
│                                                         │
│  ┌─────────────────────────────────────────────────┐   │
│  │                  Adapters                        │   │
│  │  SqliteRepositoryAdapter | ChromaVectorRepository│  │
│  │  InMemoryVectorRepository | OrtEmbedding         │   │
│  │  MockEmbedding            | TreeSitterParser     │   │
│  └─────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

## Layers

### CLI Layer (`src/main.rs`)

The command-line interface that users interact with. Responsible for:
- Parsing command-line arguments
- Initializing dependencies (wiring adapters to use cases)
- Formatting and displaying output

### Application Layer (`src/application/`)

Contains use cases and interface definitions (ports):

**Use Cases** (`src/application/use_cases/`):
- **IndexRepositoryUseCase**: Indexes a code repository
- **SearchCodeUseCase**: Performs semantic search
- **ListRepositoriesUseCase**: Lists indexed repositories
- **DeleteRepositoryUseCase**: Removes a repository from the index

**Interfaces/Ports** (`src/application/interfaces/`):
- **VectorRepository**: Interface for vector storage operations
- **RepositoryRepository**: Interface for repository metadata persistence
- **EmbeddingService**: Interface for generating embeddings
- **ParserService**: Interface for code parsing

### Domain Layer (`src/domain/`)

Pure domain objects with encapsulated behavior. All fields are private with accessor methods.

**Models** (`src/domain/models/`):
- **CodeChunk**: Represents a parsed code segment with domain methods like `line_count()`, `is_callable()`, `qualified_name()`, `preview()`
- **Repository**: Represents an indexed repository with methods like `is_indexed()`, `average_chunks_per_file()`, `summary()`
- **Embedding**: Vector representation with methods like `is_normalized()`, `magnitude()`, `cosine_similarity()`
- **SearchResult/SearchQuery**: Search-related value objects with relevance checking and filter methods
- **Language**: Programming language enum with methods like `is_known()`, `primary_extension()`, `uses_braces()`

**Error** (`src/domain/error.rs`):
- **DomainError**: Unified error type with helper methods like `is_not_found()`, `is_storage_error()`

### Connector Layer (`src/connector/`)

Implements the application interfaces with concrete adapters:

**Adapters** (`src/connector/adapter/`):
- **SqliteRepositoryAdapter**: SQLite-based repository persistence
- **ChromaVectorRepository**: ChromaDB-based vector storage
- **InMemoryVectorRepository**: In-memory vector storage for testing
- **OrtEmbedding**: ONNX Runtime embedding generation
- **MockEmbedding**: Mock embeddings for testing
- **TreeSitterParser**: Tree-sitter based code parser

## Project Structure

```text
src/
├── domain/                           # Pure domain objects
│   ├── error.rs                      # DomainError type
│   ├── mod.rs
│   └── models/
│       ├── code_chunk.rs             # CodeChunk + NodeType
│       ├── embedding.rs              # Embedding value object
│       ├── language.rs               # Language enum
│       ├── mod.rs
│       ├── repository.rs             # Repository aggregate
│       └── search_result.rs          # SearchResult + SearchQuery
│
├── application/                      # Use cases + interfaces
│   ├── interfaces/                   # Port definitions
│   │   ├── embedding_service.rs
│   │   ├── mod.rs
│   │   ├── parser_service.rs
│   │   ├── repository_repository.rs
│   │   └── vector_repository.rs
│   ├── mod.rs
│   └── use_cases/
│       ├── delete_repository.rs
│       ├── index_repository.rs
│       ├── list_repositories.rs
│       ├── mod.rs
│       └── search_code.rs
│
├── connector/                        # Adapter implementations
│   ├── adapter/
│   │   ├── chroma_vector_repository.rs
│   │   ├── in_memory_vector_repository.rs
│   │   ├── mock_embedding.rs
│   │   ├── mod.rs
│   │   ├── ort_embedding.rs
│   │   ├── sqlite_repository_adapter.rs
│   │   └── treesitter_parser.rs
│   └── mod.rs
│
├── lib.rs                            # Library exports
└── main.rs                           # CLI entry point
```

## Data Flow

### Indexing Flow

```text
Repository Path
      │
      ▼
┌─────────────┐
│  Walk Files │
└─────────────┘
      │
      ▼
┌─────────────┐
│Parse w/ TS  │ ─── TreeSitterParser
└─────────────┘     Extract functions, classes, etc.
      │
      ▼
┌─────────────┐
│  Generate   │ ─── OrtEmbedding (all-MiniLM-L6-v2)
│  Embeddings │
└─────────────┘
      │
      ├──────────────────┐
      ▼                  ▼
┌─────────────┐    ┌─────────────┐
│   SQLite    │    │  ChromaDB   │
│SqliteAdapter│    │ChromaAdapter│
└─────────────┘    └─────────────┘
```

### Search Flow

```text
Query String
      │
      ▼
┌─────────────┐
│   Embed     │ ─── OrtEmbedding
│   Query     │
└─────────────┘
      │
      ▼
┌─────────────┐
│  Vector     │ ─── ChromaVectorRepository (similarity search)
│  Search     │
└─────────────┘
      │
      ▼
┌─────────────┐
│   Fetch     │ ─── Reconstruct CodeChunk domain objects
│  Results    │
└─────────────┘
      │
      ▼
Search Results
```

## Design Decisions

### Why DDD with Ports & Adapters?

- **Clear separation**: Domain logic is isolated from infrastructure
- **Testability**: Easy to test with mock adapters
- **Flexibility**: Easy to swap implementations (e.g., different vector databases)
- **Dependency Inversion**: High-level modules don't depend on low-level modules

### Interface Location (Application Layer)

Following the Ports & Adapters pattern, interfaces (ports) are defined in the Application layer:
- Use cases depend on interfaces they own
- Adapters implement these interfaces
- Domain remains pure with no external dependencies

### Domain Objects with Behavior

Domain models encapsulate both data and behavior:
- Private fields with accessor methods
- `reconstitute()` factory for adapter use
- Domain-specific methods (e.g., `CodeChunk::is_callable()`, `Embedding::cosine_similarity()`)

### Why Tree-sitter?

- Fast, incremental parsing
- Multi-language support
- Produces concrete syntax trees
- Battle-tested in many editors

### Why ONNX Runtime?

- Rust-native embedding generation
- High-performance inference
- Supports multiple embedding models
- No Python dependency required

### Why ChromaDB?

- Purpose-built for embeddings
- Simple HTTP API
- Supports persistence
- Easy to deploy
