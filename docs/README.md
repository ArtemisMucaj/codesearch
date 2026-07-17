# CodeSearch documentation

Full reference for CodeSearch — a semantic code search, code-understanding, and
architecture-analysis tool. Start with the [project README](../README.md) for a
tour; use this index to go deep on any subsystem.

## Getting started

- [Getting Started](./features/getting-started.md) — install, index your first
  repository, run your first searches, and connect an editor.

## Search & retrieval

- [Search Features](./features/search.md) — the hybrid (semantic + keyword)
  pipeline, RRF fusion, reranking, filters, and output formats.
- [Embedding Backends](./features/embedding-backends.md) — local ONNX vs
  remote API embeddings, reranking backends, and dimension enforcement.
- [Indexing Pipeline](./features/indexing.md) — discovery → parse → embed →
  store, incremental (hash-based) re-indexing, and automatic namespace
  resolution.

## Understanding code

- [Call Graph Analysis](./features/call-graph.md) — `impact` (blast radius),
  `context` (360° caller/callee), and `explain` (LLM call-flow explanation).
- [Architecture & Dependency Analysis](./features/architecture-analysis.md) —
  execution `features`, Leiden `clusters` and `symbol-clusters`, `couplings`,
  `channels`, `overview`, `visualize`, `uses`, and the `query_graph` MCP tool.

## Serving & integrations

- [Serve & Management API](./features/serve-and-management-api.md) — the
  `serve` command, the REST/JSON + SSE management API, and how it relates to
  the MCP server.
- [Editor Integrations](./features/editor-integrations.md) — Neovim/Telescope,
  Zed (MCP context server + tasks), output formats, and the JSON schema.
- [`management-api.openapi.json`](./management-api.openapi.json) — the machine-
  readable OpenAPI contract for the management API (served at
  `GET /api/openapi.json`).

## Long-term memory

- [Long-Term Memory](./features/memory.md) — importing finished sessions, the
  four memory kinds, the `memory://` virtual filesystem, project scoping,
  dreaming (consolidation), and the memory MCP tools.

## Architecture & contributing

- [Architecture Overview](./architecture/overview.md) — the Domain-Driven
  Design / Ports & Adapters layering, use cases, adapters, and data flow.
- [AGENTS.md](../AGENTS.md) — the canonical agent & contributor guide: build,
  test, code style, commit conventions, LLM backends, and CI/CD.

## Design notes

- [`plans/`](./plans/) — historical design and implementation plans, kept for
  context on why certain subsystems look the way they do. These are point-in-
  time records, not current reference.
