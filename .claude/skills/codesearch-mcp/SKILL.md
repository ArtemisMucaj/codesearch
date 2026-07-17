---
name: codesearch-mcp
description: Semantic code search, code-understanding, and long-term memory over a codebase, via the codesearch MCP server. At the START of a session, recall the user's preferences and this project's overview through the memory tools; then find code by what it does (search_code), and trace relationships (get_symbol_context / analyze_impact / query_graph). Use when the codesearch MCP server is connected and the user asks to find code by behaviour, understand how symbols relate, explore structure, or recall what past sessions learned.
metadata:
  author: ArtemisMucaj
  version: "1.7.0"
compatibility: Requires the codesearch MCP server to be connected (e.g. `codesearch mcp` over stdio, or `codesearch serve` over HTTP). Code-search tools need the repository indexed; memory tools work as soon as any sessions have been imported.
---

# Codesearch (MCP)

The codesearch MCP server exposes semantic code search, call-graph analysis,
architecture mapping, and long-term memory as tools you can call directly. This
skill is the playbook for *when and in what order* to call them. It names each
tool and what it's for; the exact parameters are on each tool's own schema —
discover those as you go, don't guess them from here.

Four capabilities over one index:

- Recall — long-term memory from past sessions: preferences, project overview,
  experiences, facts. Load it first, every session (`read_memory`,
  `search_memory`, `list_memories`).
- Map — architecture at a glance: entry-point features, file/symbol
  communities, coupling hotspots, cross-service channels
  (`list_features`, `list_clusters`, `list_symbol_clusters`, `couplings`,
  `channels`, `file_uses`).
- Search — hybrid retrieval by meaning and exact token (`search_code`).
- Understand — call-graph relationships for a symbol (`get_symbol_context`,
  `analyze_impact`, `query_graph`).

Search by intent when you don't know the identifier. When you do have a symbol
name, `get_symbol_context` shows who calls it and what it calls, and
`analyze_impact` shows everything a change would break — real call-graph
relationships, not just where the text appears.

---

# The runbook

Follow these phases in order. Most tools take an optional repository argument;
omit it to use the connected workspace's repository, and set it only when
several repositories are indexed and you need to disambiguate.

## Phase 1 — Recall memory (do this first)

Before any substantive work, load what past sessions learned. It's cheap and
keeps you from re-asking things the user already told you or working against
their conventions.

1. `read_memory` with no arguments — returns the whole-memory digest: a single
   abstract + overview of everything known about the user and this project.
   Read this first, then drill in only where relevant.
2. `list_memories` filtered to preferences — load the user's standing
   preferences (code style, tooling, workflow) before you write or change code.
3. `search_memory` — pull anything specific to the task you're about to start
   (past decisions, a prior fix, a project fact).

`search_memory` is scoped to the connected project + globals by default. If
memory is empty (nothing imported yet) these return little — that's fine,
proceed. Don't skip the check just because it *might* be empty.

To go deeper into the memory virtual filesystem, call `read_memory` with a
directory URI (e.g. `memory://sessions`) to list its children's one-line
abstracts, then a leaf URI (e.g. `memory://sessions/<id>`) for a node's full
detail such as a past session transcript.

## Phase 2 — Get the architecture overview

Orient in the codebase before diving in. Start broad, then zoom in only if the
task needs it.

- `list_repositories` — what's indexed, with sizes and language breakdown (also
  serves as index stats).
- `list_features` — entry-point execution flows ranked by criticality: the most
  load-bearing behaviours in the repo.
- `list_clusters` — architectural modules (tightly-coupled groups of files).
- `list_symbol_clusters` — behavioural communities (groups of collaborating
  functions/types that often cut across files).
- `couplings` — the single file/symbol or edge that glues a module together
  (a refactoring target).
- `channels` — cross-service links (Kafka, HTTP, MQTT, AMQP, gRPC) between the
  indexed repositories.
- `file_uses` — which files in one repository reference another.

Drill from a listing to a specific item with `get_feature`, `get_file_cluster`,
or `get_symbol_cluster`.

## Phase 3 — Search by intent

Call `search_code` with a description of *what the code does* — include the
domain noun and the behaviour. Prefer a short phrase or question over one word.

- Good: "how are file chunks created and stored", "middleware that validates
  auth tokens before issuing a session".
- Weak: "error" (too generic — say "error handling for X"); a bare identifier
  you already know (skip search — go straight to Phase 4).

Then read the top hits: each result carries the file path, line range, symbol,
and a code preview. Read the top 3–5 at their lines to confirm before relying on
them. Treat the ranking as a lead, not a verdict.

If the first call misses, refine rather than repeat: add domain context, switch
between hybrid (default) and semantic-only, narrow by language or repository,
or widen the result limit. Rephrase using vocabulary you saw in the first batch.
(These are all parameters on `search_code` — check its schema for the names.)

> If you already have the exact symbol name, `search_code` is the wrong phase —
> jump straight to Phase 4 for its callers, callees, and blast radius.

## Phase 4 — Understand a symbol (start here when you know its name)

Once you have a symbol name — from Phase 3, or because the user named it — these
tools report where it's used and where a change lands, from the call graph:

- `get_symbol_context` — who calls it and what it calls (the immediate
  neighbourhood).
- `analyze_impact` — everything a change would transitively break (blast
  radius), so you know what needs updating and how risky it is.
- `query_graph` — a single, precise relationship at a time when you want just
  one edge kind rather than the whole neighbourhood: callers, callees, imports,
  importers, inheritors, children (supertypes), tests, or a file's symbols.
- `get_impacted_features` — which entry-point features a set of changed symbols
  touches (change → affected user-visible behaviours).

Symbol arguments match by substring by default; supply an anchored regex when
you need precision (see each tool's schema for the flag).

## Phase 5 — Change, then re-index

The call graph and architecture tools read what was captured at index time.
After you change code, re-index so those tools stay accurate. Indexing and
memory import are CLI operations, not MCP tools:

- Re-index: `codesearch index <path>` (incremental — only changed files
  re-parse).
- Record the session for next time: `codesearch memory import <transcript>`.

If the server was started with `codesearch serve`, indexing and memory import
also run over its management API and on a background schedule — but the two
commands above always work.

---

# Reference

## Tool index (by phase)

| Phase | Tools |
|---|---|
| Recall | `read_memory`, `search_memory`, `list_memories` |
| Map | `list_repositories`, `list_features` / `get_feature`, `list_clusters` / `get_file_cluster`, `list_symbol_clusters` / `get_symbol_cluster`, `couplings`, `channels`, `file_uses` |
| Search | `search_code` |
| Understand | `get_symbol_context`, `analyze_impact`, `query_graph`, `get_impacted_features` |

Parameters for each tool live on its schema — discover them at call time rather
than assuming. Prefer omitting optional filters (repository, language, limits)
unless they're needed.

## CLI-only (no MCP tool)

Some capabilities are CLI-only. When the user wants one of these, reach for the
CLI (or the [`codesearch` CLI skill](../codesearch/SKILL.md)), not an MCP tool:

- `explain` — LLM narrative of a symbol's call flow and business purpose.
- `overview` — the one-page combined Markdown dossier.
- `visualize` — HTML / SVG / Obsidian-canvas graph rendering.
- `tui` — the interactive terminal UI.
- `index`, `create`, `delete`, `memory import` / `add` / `dream` — indexing and
  memory-writing operations.

## Keywords

mcp, model context protocol, codesearch mcp, semantic code search, hybrid
search, find code, code understanding, call graph, symbol context, callers,
callees, impact analysis, blast radius, query graph, execution features,
clusters, symbol clusters, communities, coupling, cross-service channels, uses,
long-term memory, read_memory, search_memory, list_memories, recall preferences,
project overview, session start memory, remember decisions, user preferences,
project facts, search_code, get_symbol_context, analyze_impact
