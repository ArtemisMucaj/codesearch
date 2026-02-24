# CodeSearch — Agent & Contributor Guide

This file is the canonical reference for anyone (human or AI agent) working on this codebase. It covers architecture, conventions, and development workflows.

---

## Table of Contents

1. [Project Overview](#project-overview)
2. [Architecture](#architecture)
3. [Directory Structure](#directory-structure)
4. [Build, Run & Test](#build-run--test)
5. [Code Style](#code-style)
6. [Commit Style](#commit-style)
7. [Testing Strategy](#testing-strategy)
8. [Adding New Features](#adding-new-features)
9. [CI/CD](#cicd)

---

## Project Overview

CodeSearch is a semantic code search tool written in Rust. It indexes source code repositories and answers natural-language queries by combining:

- **Semantic search** — ONNX embedding models (sentence-transformers) stored in DuckDB with HNSW vector indexing.
- **Keyword search** — BM25-style SQL `LIKE` patterns over the same store.
- **Hybrid fusion** — Reciprocal Rank Fusion (RRF) to merge the two result sets.
- **Call graph analysis** — Tree-sitter AST extraction for callers/callees and blast-radius impact analysis.
- **MCP server** — Exposes search as a [Model Context Protocol](https://modelcontextprotocol.io) server so AI tools (Claude, Cursor, etc.) can call it directly.

The tool ships as a single binary with a CLI and an optional HTTP/stdio MCP server mode.

---

## Architecture

The codebase follows **Domain-Driven Design (DDD)** with a strict **Ports & Adapters** (Hexagonal) layering. Dependencies always point inward — outer layers depend on inner layers, never the reverse.

```
┌─────────────────────────────────────────────┐
│  CLI / MCP Server  (src/cli, connector/mcp) │
└───────────────────────┬─────────────────────┘
                        │ calls
┌───────────────────────▼─────────────────────┐
│  Application Layer  (src/application)        │
│  • Use Cases (orchestration logic)           │
│  • Interfaces / Ports (traits only)          │
└──────────┬────────────────────────┬──────────┘
           │ depends on             │ depends on
┌──────────▼──────────┐  ┌─────────▼───────────┐
│  Domain Layer        │  │  Connector Layer     │
│  (src/domain)        │  │  (src/connector)     │
│  • Models            │  │  • DuckDB adapters   │
│  • Error types       │  │  • ONNX adapters     │
└─────────────────────┘  │  • Tree-sitter parser│
                          │  • MCP server        │
                          │  • DI container      │
                          └──────────────────────┘
```

### Layers at a Glance

| Layer | Path | Responsibility |
|---|---|---|
| Domain | `src/domain/` | Pure value types (`CodeChunk`, `SearchResult`, `Embedding`, …). No I/O, no async, no external crates beyond `serde`. |
| Application | `src/application/` | Use cases (orchestration) and port traits (`VectorRepository`, `EmbeddingService`, …). Depends only on Domain. |
| Connector | `src/connector/` | Concrete adapter implementations, dependency-injection container, CLI router, MCP server. Depends on Application + Domain. |
| CLI | `src/cli/` | `clap`-based command definitions. Parses flags and delegates to the Router. |

### Key Use Cases

| Use Case | File |
|---|---|
| Index a repository | `src/application/use_cases/index_repository.rs` |
| Hybrid search + reranking | `src/application/use_cases/search_code.rs` |
| RRF result fusion | `src/application/use_cases/rrf_fuse.rs` |
| Impact (blast radius) analysis | `src/application/use_cases/impact_analysis.rs` |
| Symbol context (callers/callees) | `src/application/use_cases/symbol_context.rs` |
| Call graph extraction | `src/application/use_cases/call_graph.rs` |
| List / delete repositories | `src/application/use_cases/{list,delete}_repository.rs` |

### Dependency Injection

`src/connector/api/container.rs` wires every adapter to its port trait and builds the use-case objects. When adding a new adapter or use case, register it here.

`src/connector/api/router.rs` maps CLI commands to use cases and handles output formatting.

---

## Directory Structure

```
codesearch/
├── src/
│   ├── main.rs                     # Binary entry point; sets up logging and delegates to Router
│   ├── lib.rs                      # Re-exports for integration tests
│   ├── domain/
│   │   ├── models/                 # CodeChunk, Embedding, SearchResult, Repository, …
│   │   └── error.rs                # Unified CodeSearchError enum (thiserror)
│   ├── application/
│   │   ├── interfaces/             # Port traits (one file per abstraction)
│   │   └── use_cases/              # Business logic (one file per use case)
│   └── connector/
│       ├── adapter/
│       │   ├── duckdb/             # DuckDB implementations (vector, metadata, call graph, file hash)
│       │   ├── ort/                # ONNX Runtime (embedding + reranking)
│       │   ├── tree_sitter/        # Multi-language AST parser
│       │   └── mcp/                # MCP server (stdio + HTTP)
│       └── api/
│           ├── container.rs        # Dependency injection
│           └── router.rs           # CLI command routing
├── src/cli/                        # clap command structs
├── tests/
│   ├── integration_tests.rs        # Full pipeline tests (in-memory storage)
│   ├── duckdb_metadata_repository_tests.rs
│   ├── duckdb_vector_repository_tests.rs
│   └── fixtures/                   # Sample source files (Rust, Python, JS, TS, Go, …)
├── docs/
│   ├── architecture/overview.md    # Architecture diagrams and design decisions
│   └── features/                   # User-facing feature docs
├── ide/nvim/                       # Neovim/Telescope integration
├── .github/workflows/
│   ├── rust.yml                    # Build + test on every push / PR to main
│   └── release.yml                 # Automated cross-platform release builds
├── Cargo.toml
└── CHANGELOG.md                    # Kept by release-please (Conventional Commits)
```

---

## Build, Run & Test

### Prerequisites

- Rust toolchain (stable, managed via `rustup`). The project uses Edition 2021.
- No system-level native dependencies — DuckDB and ONNX Runtime are bundled via Cargo features.

### Build

```bash
# Development build (fast compile, unoptimised)
cargo build

# Optimised release build (LTO, stripped symbols, single codegen unit)
cargo build --release

# The binary lands at:
./target/release/codesearch
```

### Run

```bash
# Index the current repository
cargo run --release -- index .

# Search for something
cargo run --release -- search "error handling for network timeouts"

# List indexed repositories
cargo run --release -- list

# Start an MCP server on stdio (for AI tool integrations)
cargo run --release -- mcp

# Start an MCP server over HTTP
cargo run --release -- mcp --http 3000
```

Or after copying the release binary to your `$PATH`:

```bash
codesearch index .
codesearch search "embedding similarity"
codesearch impact MyStruct --depth 5
codesearch context some_function
```

### Lint & Format

```bash
# Apply canonical Rust formatting (required before committing)
cargo fmt

# Run Clippy (treat warnings as guidance — fix anything Clippy flags)
cargo clippy

# Run both together
cargo fmt && cargo clippy
```

### Test

```bash
# Run the full test suite
cargo test

# Run with stdout visible (useful when debugging)
cargo test -- --nocapture

# Run a single test by name (substring match)
cargo test test_search_returns_relevant_results

# Run only integration tests
cargo test --test integration_tests

# Run only one of the repository adapter test suites
cargo test --test duckdb_metadata_repository_tests
cargo test --test duckdb_vector_repository_tests
```

Tests use in-memory DuckDB storage and mock embeddings so they run without a live model or persistent database.

---

## Code Style

### General Rust Conventions

- Follow standard Rust idioms as enforced by `cargo fmt` (rustfmt defaults) and `cargo clippy`.
- Prefer `?` for error propagation — do not use `.unwrap()` or `.expect()` in library code. In tests, `.unwrap()` is acceptable when failure should panic immediately.
- Avoid `clone()` where borrowing suffices. Prefer returning owned values from constructors and passing references into functions.
- Use `async`/`await` throughout. All I/O is async; blocking calls must be wrapped in `tokio::task::spawn_blocking`.
- Name types with `PascalCase`, functions and variables with `snake_case`, constants with `SCREAMING_SNAKE_CASE`.

### Error Handling

- Domain errors live in `src/domain/error.rs` as variants of `CodeSearchError` (via `thiserror`).
- Use `anyhow::Context` in application and connector layers to annotate errors with call-site context before propagating.
- Do not swallow errors silently. Log with `tracing::warn!` or `tracing::error!` before dropping an error, if you must.

### Logging & Tracing

- Use the `tracing` macros (`trace!`, `debug!`, `info!`, `warn!`, `error!`).
- Instrument async functions with `#[tracing::instrument]` where the span is meaningful for debugging.
- Do not use `println!` or `eprintln!` in library code — only in CLI output formatting inside the router.

### Async

- All port traits in `src/application/interfaces/` use `#[async_trait]`.
- Do not hold `MutexGuard` or other non-`Send` types across `.await` points.

### Module Organisation

- One logical concept per file. If a file grows beyond ~300 lines, split it.
- Keep domain models free of business logic — logic belongs in use cases.
- Port traits belong in `src/application/interfaces/`, not in the domain layer.

### No Magic Numbers

Define named constants for any numeric value that isn't immediately obvious (e.g. default result counts, score thresholds, search depths).

---

## Commit Style

This project follows the [Conventional Commits](https://www.conventionalcommits.org/) specification. The `CHANGELOG.md` is generated automatically by `release-please` from commit messages, so correct commit types are important.

### Format

```
<type>(<optional scope>): <short description>

[optional body — wrap at 72 characters]

[optional footer: Fixes #<issue>, BREAKING CHANGE: …]
```

### Types

| Type | When to use |
|---|---|
| `feat` | A new user-visible feature |
| `fix` | A bug fix |
| `refactor` | Code restructuring with no behaviour change |
| `perf` | Performance improvement |
| `docs` | Documentation only |
| `test` | Adding or correcting tests |
| `chore` | Build tooling, dependency updates, release bookkeeping |
| `ci` | Changes to GitHub Actions workflows |

### Examples

```
feat: add PHP language support via tree-sitter-php

fix: handle empty query string in hybrid search gracefully

refactor: extract RRF fusion into dedicated use case module

docs: update MCP server configuration examples in README

chore: bump duckdb to 1.5.0
```

### Rules

- Use the **imperative mood** in the description ("add", "fix", "remove" — not "added", "fixes", "removes").
- Keep the subject line under **72 characters**.
- Reference GitHub issues or PRs in the footer when applicable: `Fixes #42`, `Closes #17`.
- Mark breaking changes with `BREAKING CHANGE:` in the footer (or `!` after the type: `feat!:`).

---

## Testing Strategy

### Principles

- **Integration over unit** — the project favours integration tests that exercise the full pipeline (parse → embed → store → search) using in-memory storage and mock embeddings. This validates the wiring of the DI container without requiring a live model.
- **Deterministic tests** — use `--mock-embeddings` / the `InMemoryVectorRepository` in tests so results are reproducible and fast.
- **No network in tests** — tests must not call external services (HuggingFace Hub, Anthropic API). Mock or skip anything that requires outbound connections.

### Writing a New Integration Test

1. Add it to `tests/integration_tests.rs` (or a new file under `tests/` for a specific adapter).
2. Use `ContainerConfig` with `memory_storage: true` and `mock_embeddings: true`.
3. Use `tempfile::TempDir` for any file-system fixtures.
4. Annotate with `#[tokio::test]` (the test binary is configured for a multi-thread runtime).

### Fixtures

Add sample source files under `tests/fixtures/` when a test needs real parseable code. Name them `<language>_sample.<ext>` and keep them short (< 100 lines).

---

## Adding New Features

### Adding a New Language

1. Add the `tree-sitter-<lang>` crate to `Cargo.toml`.
2. Register the grammar in `src/connector/adapter/tree_sitter/` (follow the pattern of existing language implementations).
3. Add a fixture file under `tests/fixtures/`.
4. Add or extend a test in `tests/integration_tests.rs` to verify parsing round-trips correctly.

### Adding a New Use Case

1. Define the port traits it needs in `src/application/interfaces/` (or reuse existing ones).
2. Implement the use case in `src/application/use_cases/<name>.rs`.
3. Wire it in `src/connector/api/container.rs`.
4. Add a CLI command struct in `src/cli/` and route it in `src/connector/api/router.rs`.
5. Write an integration test.

### Adding a New Adapter

1. Create the adapter under `src/connector/adapter/<name>/`.
2. Implement the relevant port trait from `src/application/interfaces/`.
3. Register it in `src/connector/api/container.rs` behind a feature flag or config option if it's an alternative backend.

---

## CI/CD

### Workflows

| Workflow | Trigger | What it does |
|---|---|---|
| `rust.yml` | Push / PR to `main` | `cargo build --verbose` + `cargo test --verbose` |
| `release.yml` | Push to `main` | Runs `release-please` to bump versions and generate changelogs; builds cross-platform release binaries (Linux x86\_64, macOS aarch64, Windows x86\_64) and uploads them as GitHub release assets |

### Release Process

Releases are fully automated via [release-please](https://github.com/googleapis/release-please):

1. Merge conventional-commit PRs into `main`.
2. `release-please` opens a "Release PR" with an updated `CHANGELOG.md` and bumped version in `Cargo.toml`.
3. Merging the Release PR triggers the release build, creates a git tag, and publishes GitHub release assets with SHA-256 checksums.

Do not manually edit `CHANGELOG.md` or bump `Cargo.toml` version numbers — let release-please handle this.
