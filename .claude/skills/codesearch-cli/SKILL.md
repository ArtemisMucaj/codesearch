---
name: codesearch-cli
description: Use before implementing a feature, refactoring, fixing a bug, or changing any code, and whenever you need to understand how code relates — where something is handled, what calls a function, what a change would break, how modules depend. Load it first to find the right code and the blast radius before you edit. Traces relationships and recalls project memory that reading files alone misses.
metadata:
  author: ArtemisMucaj
  version: "1.7.0"
compatibility: Requires the codesearch binary installed. Code search needs the repository indexed with `codesearch index`; memory recall works as soon as any sessions have been imported.
---

# Codesearch

A CLI that gives an AI assistant four capabilities over one index:

- Recall — long-term memory from past sessions: user preferences, project
  overview, experiences, and facts. Load it first, every session.
- Map — architecture at a glance: a one-page `overview`, modules and
  communities (`clusters`, `symbol-clusters`), coupling hotspots, entry-point
  `features`, and cross-service `channels`.
- Search — hybrid retrieval (ML embeddings + keyword, fused via RRF, then
  reranked). Finds code by meaning *and* exact token in one query.
- Understand — a call graph powers caller/callee `context`, blast-radius
  `impact`, and LLM call-flow `explain`.

Search by intent when you don't know the identifier. When you do have a symbol
name, `codesearch context <symbol>` shows who calls it and what it calls, and
`codesearch impact <symbol>` shows everything a change would break — real
call-graph relationships, not just where the text appears.

---

# The runbook

Follow these phases in order. Run every command from inside the repository —
codesearch auto-resolves the namespace and embedding config from the repo's git
remote, so you almost never need `--namespace` or any embedding flags.

## Phase 1 — Recall memory (do this first)

Before any substantive work, load what past sessions learned. It is cheap and
keeps you from re-asking things the user already told you or working against
their conventions.

```shell
# The "read this first" digest across all memory (project + preferences overview)
codesearch memory show memory://memory

# The user's standing preferences (code style, tooling, workflow)
codesearch memory list --kind preference

# Anything specific to the task you're about to start
codesearch memory search "how do we handle <the thing you're about to touch>"
```

`memory search` is auto-scoped to this project + globals. If memory is empty
(nothing imported yet) these return little — that's fine, proceed. Don't skip
the check just because it *might* be empty. (Full memory reference below.)

## Phase 2 — Get the architecture overview

Orient in the codebase before diving in. Start broad, then zoom in only if the
task needs it.

```shell
codesearch overview                       # one-page dossier: modules, communities,
                                          #   couplings, critical features, channels
```

Zoom in when relevant:

```shell
codesearch features list                  # entry-point flows ranked by criticality
codesearch clusters list                  # architectural modules (file-level)
codesearch symbol-clusters list           # behavioural communities (symbol-level)
codesearch couplings                      # what single element glues a module together
codesearch channels                       # cross-service Kafka/HTTP/MQTT/AMQP/gRPC links
codesearch uses <from> <to>               # files in one repo that reference another
codesearch visualize -o graph.html        # interactive community graph
```

`overview` caches its analysis and refreshes automatically when you re-index.
Add `-r/--repository` if several repos are indexed.

## Phase 3 — Search by intent

Describe *what the code does* — include the domain noun and the behaviour.
Prefer a short phrase or question over one word.

```shell
# Good — behaviour + domain
codesearch search "how are file chunks created and stored"
codesearch search "middleware that validates auth tokens before issuing a session"

# Weak — fix by choosing the right tool
codesearch search "error"           # too generic → "error handling for X"
codesearch search "HandleRequest"   # you already know the symbol → skip search;
                                    #   go to Phase 4 (context / impact) instead
```

Then read the top hits (each result has `file_path`, line range, symbol, and
a preview): search → Read the top 3–5 at their lines → confirm. Treat the
ranking as a lead, not a verdict.

> If you already have the exact symbol name, `search` is the wrong phase — jump
> straight to Phase 4 (`context` / `impact`) to see its callers, callees, and
> blast radius.

If the first query misses, refine rather than repeat:

```shell
codesearch search "..." --no-text-search       # semantic-only: abstract intent, no exact tokens
codesearch search "..." --language rust         # narrow by language (repeatable)
codesearch search "..." --repository my-project # narrow by repo (repeatable)
codesearch search "..." --num 25                # widen the result set (default 10)
codesearch search "..." --format json           # structured output when you'll parse it
```

Rephrase using vocabulary you saw in the first batch. Scoring note: hybrid RRF
scores are ~0.016–0.033; semantic-only cosine scores are 0.0–1.0 — set
`--min-score` to match the mode.

## Phase 4 — Understand a symbol (start here when you know its name)

Once you have a symbol name — from Phase 3, or because the user named it — this
is how you learn where it's used and where a change lands. These query the call
graph, so they report real callers, callees, and blast radius.

```shell
codesearch context authenticate     # who calls it / what it calls (immediate neighbourhood)
codesearch impact authenticate      # everything a change transitively breaks (blast radius)
codesearch explain authenticate     # LLM narrative: purpose, data/control flow, business feature
```

Use `context` to see callers and callees, `impact` to find every place a change
would ripple to (so you know what needs updating and how risky it is), and
`explain` when you need the *why*, not just the edges.

Symbols resolve by substring by default; pass `--regex` to supply a POSIX regex
used as-is (anchor it yourself when you need an exact match). Common flags:
`-r/--repository`, `-F/--format`
(`text`/`json`/`vimgrep`; `explain` has no `vimgrep`).

```shell
codesearch impact "^MyNs/.*Service#get$" --regex
codesearch features impacted authenticate hash_password   # which features a change touches
```

## Phase 5 — Change, re-index, record

After editing, keep the index (and thus the call graph and architecture
analysis) in sync, and capture what you learned:

```shell
codesearch index <path>                     # incremental — only changed files re-parse
codesearch memory import <transcript.jsonl> # distill this session for next time
```

---

# Reference

## First-time setup

```shell
# Install the binary if it's missing (also installs scip-php / scip-typescript
# for precise PHP / JS / TS call graphs)
INSTALL_DIR="$HOME/.local/bin" sh .claude/skills/codesearch-cli/install.sh
codesearch --version   # ensure ~/.local/bin is on PATH

# Index the repository (run once; incremental afterward)
codesearch index /path/to/repo
codesearch index /path/to/repo --force   # full re-index, ignore cached hashes
```

Supported languages: Rust, Python, JavaScript, TypeScript, Go, HCL/Terraform,
PHP, C++. Indexing extracts functions, methods, classes/structs/enums, traits,
impls, modules, constants, typedefs, and imports.

## Memory in depth

Four kinds: preference (how the user likes to work), fact (project facts
and decisions), experience (a reusable insight — trigger, approach,
guardrails), and skill (a reusable procedure).

Recall (Phase 1) — more ways to read:

```shell
codesearch memory list --kind fact                  # project facts & decisions
codesearch memory search "deploy steps" --kind skill
codesearch memory search "..." --project <name>     # another project (or --all-projects)
codesearch memory tree                              # browse the memory:// virtual filesystem
codesearch memory sessions                          # what past sessions have been imported
codesearch memory tree memory://sessions            # recent sessions, one-line abstracts each
codesearch memory show memory://sessions/<id>       # a past session's transcript
codesearch memory show experience/<name>            # one item by kind/name
```

Resuming work: to pick up where recent sessions left off, read the digest
(`memory show memory://memory`), then list recent sessions
(`memory tree memory://sessions`) and read the latest one or two
(`memory show memory://sessions/<id>`) for their agent-loop context before
touching code.

`memory://memory` is the digest across all memory; `memory://projects/<project>`
is one project's overview. `memory search` auto-scopes to the current project +
globals; `memory list` lists all items of a kind, newest first.

Record (Phase 5) — more ways to write:

```shell
codesearch memory add ./docs/design.md               # store a file as a summarized resource
codesearch memory add https://example.com/g --name g # store a URL
codesearch memory dream                              # consolidate: merge dupes, resolve conflicts
```

## Interactive TUI

```shell
codesearch tui                      # search mode
codesearch tui --mode impact        # impact mode
codesearch tui --query "auth flow"  # pre-populate and dispatch
```

## Repository & namespace management

```shell
codesearch list                     # indexed repositories in the namespace
codesearch stats                    # index statistics
codesearch delete <id-or-path>      # remove a repository
```

Namespace resolution is automatic from the repo's git remote — an explicit
`--namespace` overrides it. Rarely needed global flags:

```shell
codesearch --namespace my-project search "query"    # force a namespace
codesearch --data-dir /custom/path search "query"   # custom index location
codesearch --no-rerank search "query"               # skip reranking (faster)
codesearch --memory-storage search "query"          # ephemeral, no persistence
```

## Keywords

semantic search, hybrid search, code search, natural language search, find code,
explore codebase, code understanding, intent search, AST analysis, embeddings,
code discovery, BM25, keyword search, RRF, reciprocal rank fusion, reranking,
call graph, impact analysis, blast radius, symbol context, callers, callees,
explain, call flow, execution features, criticality, clusters, modules,
architecture overview, dossier, Leiden, community detection, symbol clusters,
communities, coupling, hub dependency, cross-service channels, kafka, cross-
repository dependencies, uses, visualize, graph, TUI, regex symbol match,
long-term memory, recall preferences, project overview, session start memory,
remember decisions, user preferences, project facts
