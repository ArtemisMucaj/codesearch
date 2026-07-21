---
name: codesearch-mcp
description: Use before implementing a feature, refactoring, fixing a bug, or changing any code, and whenever you need to understand how code relates — where something is handled, what calls a function, what a change would break, how modules depend. Load it first to find the right code and the blast radius before you edit. Traces relationships and recalls project memory that reading files alone misses.
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
- Map — architecture at a glance: a one-shot dossier (`overview`), entry-point
  features, file/symbol communities, coupling hotspots, cross-service channels
  (`overview`, `list_features`, `list_clusters`, `list_symbol_clusters`,
  `couplings`, `channels`, `file_uses`).
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

Resuming work: to pick up where recent sessions left off, read the digest
(`read_memory` with no args), then `read_memory("memory://sessions")` and drill
into the latest one or two for their agent-loop context before touching code.

When a task turns up a durable reference worth keeping — a design doc, a spec, a
guide URL — store it with `add_memory_resource` (a file path or URL) so a later
session can recall it. It's summarised and saved under `memory://resources`.

## Phase 2 — Get the architecture overview

Orient in the codebase before diving in. Start broad, then zoom in only if the
task needs it.

- `overview` — the fastest way to orient: one call returns the whole dossier —
  index stats, architectural modules, symbol communities, coupling hotspots,
  critical features, and cross-service channels. Read this first; each section
  is `null` if it couldn't be computed (with a reason under `skipped`).
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

## Phase 5 — Keep results current after a change

The call graph and architecture tools reflect the index as of the last time the
repository was indexed. After a substantial change, the newest code may not be
reflected yet, so cross-check anything critical against the file you just edited.

The server keeps the index fresh for you: when it was launched to also run the
management API, it re-indexes and consolidates memory in the background on a
schedule, so the tools converge on the current code without any action from you.
If you need the very latest state immediately and the tools look stale, ask the
user to re-index, then re-run the tool.

---

# Reference

## Tool index (by phase)

| Phase | Tools |
|---|---|
| Recall | `read_memory`, `search_memory`, `list_memories`; `add_memory_resource` to store a file/URL for later recall |
| Map | `overview`, `list_repositories`, `list_features` / `get_feature`, `list_clusters` / `get_file_cluster`, `list_symbol_clusters` / `get_symbol_cluster`, `couplings`, `channels`, `file_uses` |
| Search | `search_code` |
| Understand | `get_symbol_context`, `analyze_impact`, `query_graph`, `get_impacted_features` |

Parameters for each tool live on its schema — discover them at call time rather
than assuming. Prefer omitting optional filters (repository, language, limits)
unless they're needed.

## Composing tools

Most questions are answered by combining a few tool calls rather than one.

- Repository dossier — for "give me an overview of this repo", call `overview`
  once; it returns modules, communities, couplings, critical features, and
  channels together. Drill into a section with its specific tool (`list_clusters`,
  `couplings`, …) only when the user wants more than the dossier's ranked rows.
- Explain a symbol — the call graph gives you the material to explain a symbol
  in your own words: `get_symbol_context` for its neighbourhood, then
  `search_code` on the symbol name to pull its source, then narrate the purpose
  and flow from what you read. Widen with `query_graph` (`callers_of` /
  `callees_of`) hop by hop when the chain is deep.
- Assess a change — before proposing an edit, `analyze_impact` for the blast
  radius and `get_impacted_features` for the user-visible behaviours affected;
  report both so the user sees the risk.
- Locate then understand — `search_code` to find an unknown symbol, then the
  Phase 4 tools on the symbol name it returns. Skip the search when you already
  know the name.

## Getting good results

- Start every task at Phase 1: call `read_memory` (no arguments) for the digest,
  then `list_memories` for preferences, before acting.
- Prefer omitting optional filters (repository, language, limits) unless they're
  needed; add them only to disambiguate or narrow a noisy result set.
- Read tool output before relying on it — treat rankings and matches as leads,
  and open the referenced files/lines to confirm.
- Discover each tool's parameters from its schema at call time; don't assume
  argument names or invent filters that may not exist.

## Keywords

mcp, model context protocol, codesearch mcp, semantic code search, hybrid
search, find code, code understanding, call graph, symbol context, callers,
callees, impact analysis, blast radius, query graph, execution features,
clusters, symbol clusters, communities, coupling, cross-service channels, uses,
repository overview, dossier, overview tool, long-term memory, read_memory,
search_memory, list_memories, add_memory_resource, store resource, remember a
doc, recall preferences, project overview, session start memory, remember
decisions, user preferences, project facts, search_code, get_symbol_context,
analyze_impact
