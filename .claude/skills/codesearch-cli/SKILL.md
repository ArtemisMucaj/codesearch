---
name: codesearch-cli
description: Use before implementing a feature, refactoring, fixing a bug, or changing any code, and whenever you need to understand how code relates — where something is handled, what calls a function, what a change would break, how modules depend. Load it first to find the right code and the blast radius before you edit. Traces relationships that reading files alone misses.
metadata:
  author: ArtemisMucaj
  version: "2.0.0"
compatibility: Requires the codesearch binary installed and the repository indexed with `codesearch index`.
---

# Codesearch

A CLI that gives an AI assistant three capabilities over one index:

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
codesearch resolves the namespace and embedding config automatically from the
repo's git remote, so you rarely need `--namespace`. On a **first** index the
namespace is *created* from that same remote, one per repository. Pass
`--namespace` only to deliberately share one namespace across several
repositories. See Phase 0.

## Phase 0 — First index

Everything below depends on an index existing. Do this first.

### a. Check whether the repo is already indexed

```shell
codesearch list          # indexed repositories in the current namespace
codesearch stats         # index statistics
```

If it appears, skip to Phase 1. Re-indexing is incremental and cheap; a first
index is not.

### b. Survey existing namespaces, then ask only if there is a choice

```shell
codesearch namespaces    # every namespace, its repos and embedding config
```

If no other namespace exists (fresh install, or this is the first repo ever
indexed), there is nothing to choose between — use the derived per-repo
namespace and proceed without asking.

The threshold is "another namespace exists", not "another repository exists":
a second repo inside the namespace this repo would derive is not a competing
option.

### c. When there is a choice, propose a contextual match

Do not pick silently — the first index fixes that namespace's embedding model
and dimensions permanently.

First check whether an existing namespace is a contextual fit: it holds sibling
services of the same system, other repos from the same owner, or repos this one
talks to via `channels` / `uses`. Judge from the namespace name and its
repositories.

Then present the options concretely:

- **The derived per-repo namespace** (e.g. `owner/repo`) — recommended default.
  Best search quality; nothing unrelated dilutes the ranking.
- **An existing namespace that fits** — name it and say why ("`my-system`
  already contains `svc-a` and `svc-b`, which this repo appears to talk to").
  Required for cross-repo `uses`, `channels` or namespace-wide clustering.
- **A new shared namespace** — when several related repos belong together but
  none is indexed yet.

If nothing looks like a fit, say so and recommend the derived one rather than
listing everything for its own sake.

State the consequences when asking:

- Mixing **unrelated** repositories into one namespace degrades search quality —
  retrieval and reranking compete across everything in it.
- A namespace's embedding model and dimensions are fixed by its first index and
  cannot be changed without deleting and re-indexing.

```shell
codesearch index /path/to/repo                          # derived per-repo namespace
codesearch --namespace my-system index /path/to/repo    # deliberate shared namespace
```

If the user declines to choose or does not respond, fall back to the derived
per-repo namespace and say so — that is the safe default.

### d. Creating a namespace with a specific embedding config

```shell
# Defaults: --embedding-target onnx, all-MiniLM-L6-v2, 384 dimensions.
codesearch create my-system
codesearch create my-system --embedding-target api --embedding-model <name> --embedding-dimensions <n>
codesearch create my-system --no-embeddings   # keyword + call-graph only, no model download

codesearch --namespace my-system index /path/to/repo
```

`--no-embeddings` is the fast path when only keyword search and the call graph
are needed — it skips model download and inference entirely.

### e. Indexing is long-running. Wait for it.

**A first index parses and embeds every supported file; on a large repo this
takes several minutes.** Run it in the foreground and let it finish — do not
background it, do not add a short timeout, and do not start querying while it
runs.

Querying a partially built index returns empty or misleading results, and you
may wrongly conclude the repository has no matching code.

When invoking `codesearch index` through a shell tool, set a generous timeout
(10+ minutes for a first index of an unknown repository). If the tool reports a
timeout, indexing may still be running — check `codesearch stats` before
re-running.

Subsequent indexes are **incremental** (only changed files re-parse) and are
normally fast.

## Phase 1 — Get the architecture overview

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

## Phase 2 — Search by intent

Describe *what the code does* — include the domain noun and the behaviour.
Prefer a short phrase or question over one word.

```shell
# Good — behaviour + domain
codesearch search "how are file chunks created and stored"
codesearch search "middleware that validates auth tokens before issuing a session"

# Weak — fix by choosing the right tool
codesearch search "error"           # too generic → "error handling for X"
codesearch search "HandleRequest"   # you already know the symbol → skip search;
                                    #   go to Phase 3 (context / impact) instead
```

Then read the top hits (each result has `file_path`, line range, symbol, and
a preview): search → Read the top 3–5 at their lines → confirm. Treat the
ranking as a lead, not a verdict.

> If you already have the exact symbol name, `search` is the wrong phase — jump
> straight to Phase 3 (`context` / `impact`) to see its callers, callees, and
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

## Phase 3 — Understand a symbol (start here when you know its name)

Once you have a symbol name — from Phase 2, or because the user named it — this
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

## Phase 4 — Change, then re-index

After editing, keep the index (and thus the call graph and architecture
analysis) in sync:

```shell
codesearch index <path>   # incremental — only changed files re-parse
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

# Survey namespaces before a first index (see Phase 0)
codesearch namespaces
```

**Indexing is a long-running foreground command — wait for it to finish.** A
first index takes several minutes on a large repo. Do not background it, do not
set a short timeout, and do not query a half-built index. Subsequent indexes are
incremental and fast.

Supported languages: Rust, Python, JavaScript, TypeScript, Go, HCL/Terraform,
PHP, C++. Indexing extracts functions, methods, classes/structs/enums, traits,
impls, modules, constants, typedefs, and imports.

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
repository dependencies, uses, visualize, graph, TUI, regex symbol match
