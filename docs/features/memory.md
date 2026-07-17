# Long-Term Memory

CodeSearch can import **finished assistant sessions** (e.g. Claude Code
transcripts) and distill them into durable, searchable memories: user
preferences, reusable experiences, procedural skills, and project facts. The
design uses declarative memory kinds, an LLM extraction pass over the
transcript with existing memories prefetched for in-place merging, and
rewrite-merge semantics, all fitted to CodeSearch's hexagonal layering and
DuckDB storage.

## Storage

Memory lives in its **own DuckDB file**, `~/.codesearch/memory.duckdb`,
separate from the code index (`codesearch.duckdb`):

| Table | Contents |
|---|---|
| `memory_items` | One row per memory (`kind`, `name`, Markdown `content`, provenance, timestamps, update count). Unique per `(kind, name)`. |
| `memory_vectors` | `FLOAT[dims]` embedding per item for semantic search. |
| `memory_sessions` | Imported-session markers (idempotence + audit). |
| `memory_nodes` | Virtual-filesystem nodes (`uri`, `kind`, `parent_uri`, L0 `abstract`, L1 `overview`, L2 `content`). Holds the whole-memory digest and one node per imported session. |
| `memory_node_vectors` | `FLOAT[dims]` embedding per node (its L0/L1 summary) for semantic recall. |
| `memory_meta` | Embedding model + dimensions the store was created with; mismatched opens are rejected. |

Because it is a separate file, session imports never contend with indexing,
and you can inspect or reset memory independently:

```bash
duckdb ~/.codesearch/memory.duckdb "SELECT kind, name, update_count FROM memory_items"
rm ~/.codesearch/memory.duckdb   # start over
```

## Memory kinds

| Kind | What it captures | Content shape |
|---|---|---|
| `preference` | What the user likes/dislikes or is accustomed to (code style, tooling, workflow). One topic per item. | Free-form Markdown |
| `experience` | A generalizable insight from the session — trigger, working approach, and guardrails. | `## Situation` / `## Approach` / `## Reflect` sections |
| `skill` | Reusable procedural knowledge that could become an automated skill (release flows, debugging recipes). | Best for / Flow / Prerequisites / Common failures / Recommendation |
| `fact` | Durable declarative information (project facts, decisions and rationale, environment details). | Short Markdown statement |

## Importing a session

```bash
# Claude Code session transcript
codesearch memory import ~/.claude/projects/<project>/<session-id>.jsonl

# Generic chat log: one {"role": "...", "content": "..."} JSON object per line
codesearch memory import ./session.jsonl

# Re-run extraction for a session that was already imported
codesearch memory import ./session.jsonl --force
```

The importer:

1. **Parses** the transcript (Claude Code event format or generic JSONL).
   User/assistant text is kept; tool calls become one-line `ToolCall:`
   summaries (evidence for experience/skill extraction); tool results, meta
   lines, and slash-command envelopes are dropped.
2. **Prefetches** the most similar existing memories (semantic search over
   `memory.duckdb`) so the model merges new information into existing items
   instead of duplicating them.
3. **Extracts** by sending one prompt to the configured chat model, which
   returns a single JSON object of upsert/delete operations. A malformed
   response gets one format-correction retry.
4. **Applies** the operations: names are normalized to snake_case, items are
   re-embedded, updates preserve the item's identity and bump its
   `update_count`, and the session is recorded so re-imports are no-ops
   without `--force`.

Imports are idempotent per session ID (taken from the transcript's
`sessionId`, falling back to the file name).

### Choosing the extraction model

Extraction is a summarization-style task — a **small model is enough** and
keeps imports fast and cheap. The `--llm` flag selects the provider
(`anthropic` default, or `open-ai`), configured through the same environment
variables as `explain` and query expansion:

```bash
# Local-first default: LM Studio on http://localhost:1234 (no key needed)
codesearch memory import session.jsonl

# Anthropic cloud with a small model
ANTHROPIC_BASE_URL=https://api.anthropic.com \
ANTHROPIC_API_KEY=sk-ant-... \
ANTHROPIC_MODEL=claude-haiku-4-5 \
codesearch memory import session.jsonl

# Any OpenAI-compatible server
OPENAI_BASE_URL=http://localhost:1234 OPENAI_MODEL=qwen/qwen3.5-4b \
codesearch memory import session.jsonl --llm open-ai
```

## Virtual filesystem (L0 / L1 / L2)

Beyond the flat items, memory is also navigable as a `memory://` virtual
filesystem. Every node bundles three context levels for one location:

| Level | Field | What it holds |
|---|---|---|
| **L0** | `abstract` | a one-line summary — what recall returns and ranks on |
| **L1** | `overview` | a paragraph/outline to orient before reading |
| **L2** | `content` | the full detail (e.g. a session's transcript) |

The tree has four top-level kinds — `memory` / `project` / `session` /
`resource` context types:

```text
memory://memory                 ← the whole-memory digest ("read this first")
memory://projects/<project>     ← digest of one project/namespace
memory://sessions/<id>          ← one imported session (transcript = L2)
memory://resources/...          ← files/URLs added explicitly (reserved)
```

Two things are summarized on **every import**, each with one small LLM call
(the same chat model extraction uses), a single format-recovery retry, and a
deterministic fallback so a flaky model never blocks the import or loses data:

1. **The session** → a node at `memory://sessions/<id>` whose L2 is the full
   normalized transcript (so the conversation can be re-read later), plus a
   generated L0 abstract and L1 overview.
2. **The whole memory store** → the `memory://memory` digest is regenerated
   from the current set of items: an abstract + overview meant to be read
   first, before drilling into individual memories. With fewer than two items
   this is a deterministic placeholder (no LLM call).

Per-project digests (`memory://projects/<project>`, one per distinct project
or namespace found on stored items) are also refreshed on import and during
dream cycles. Each is only regenerated when one of its project's items actually
changed, and a digest whose project vanished (all items deleted or generalized
to global) is removed.

Resources — files and website links — are added explicitly with `memory add`.
The content is fetched (URLs and HTML are decluttered to Markdown via the
[`defuddle`](https://github.com/kepano/defuddle-cli) CLI; plain text files are
read as-is), summarized into an L0/L1 the same way, and stored at
`memory://resources/<name>` with the full text as L2:

```bash
codesearch memory add ./notes/architecture.md           # a local file
codesearch memory add https://example.com/guide --name guide   # a URL
```

`defuddle` must be on `PATH` for URLs and HTML (`npm install -g defuddle`).

Browse and drill in from the CLI:

```bash
codesearch memory tree                        # roots: digest + sessions
codesearch memory tree memory://sessions      # list stored sessions (L0 lines)
codesearch memory show memory://memory        # the digest abstract + overview
codesearch memory show memory://sessions/<id> # a session's abstract + transcript
```

## Project & namespace assignment

Every memory item is either **global** (applies everywhere) or carries a
**project** it belongs to, so one project's conventions never surface as advice
in another. A session's project is resolved from its working directory when the
transcript is imported:

1. **Indexed under a user-created namespace** → the project is the *namespace*.
   Repositories deliberately indexed together in a namespace are correlated —
   they work together — so their sessions share one memory pool.
2. **Has a git remote** (not indexed, or indexed under the default namespace) →
   the project is the normalized remote (e.g. `github.com/owner/repo`). The
   remote survives clones, moves, and renames, and is the same key indexing
   matches on — so memories written *before* a repo is indexed still line up
   with sessions run *after*, instead of being orphaned.
3. **Namespace inferred from the directory tree** → when the session ran in a
   directory that contains (or sits inside) indexed repositories that all
   belong to one user-created namespace, the session is attributed to that
   namespace. If indexed repos along the path span *different* namespaces the
   result is ambiguous, so nothing is inferred.
4. **Nothing stable to key on** → the session is **global**. A bare directory
   name is a weak, collision-prone key that stops matching the moment the
   directory is indexed, so an un-inferable location contributes global
   memories rather than a throwaway project.

Recall applies the same idea in reverse: a project-filtered search returns that
project's items *plus* globals. `codesearch memory search` resolves the project
from the directory it runs in automatically; `--project <name>` overrides it and
`--all-projects` disables the filter. The extraction prefetch is filtered the
same way, so session imports merge new information into the memories that are
actually about the same project.

## Recalling memories

```bash
# Hybrid search (semantic + keyword, fused with RRF); results are filtered to
# the current directory's project + globals automatically
codesearch memory search "how do we handle duckdb lock conflicts"

# Search another project's memory, or everything
codesearch memory search "deploy steps" --project backend-team
codesearch memory search "deploy steps" --all-projects

# Restrict to one kind
codesearch memory search "code style" --kind preference

# Browse
codesearch memory list
codesearch memory list --kind experience -F json

# Full content of one item (by ID or by kind/name)
codesearch memory show experience/duckdb_lock_conflict_fix

# Housekeeping
codesearch memory sessions          # what has been imported
codesearch memory delete <item-id>  # remove one item
```

Search embeds the query with the same embedding backend as the code index,
runs a cosine-similarity leg over `memory_vectors` plus a keyword leg over
names/content, and fuses both rankings with Reciprocal Rank Fusion. When the
store was created without embeddings, search degrades to the keyword leg.

## MCP tools

When running as an MCP server (`codesearch mcp`), memory recall is exposed to
AI tools alongside code search:

| Tool | Description |
|------|-------------|
| `search_memory` | Hybrid recall over the memory store. Accepts `query`, optional `kind`, `project` (defaults to the workspace's project in stdio mode; `"*"` searches all projects), and `limit`. Returns full item content with fused scores. |
| `list_memories` | List stored memories, newest first. Accepts optional `kind` — e.g. `kind="preference"` at session start to load every known user preference. |
| `read_memory` | Read the virtual filesystem level by level. Call with no args (or `uri="memory://memory"`) first for the whole-memory digest, then drill into a directory (`memory://sessions`) or a leaf (`memory://sessions/<id>`). Returns the node's L0/L1/L2 plus its children's abstracts. |

This gives agents the recall half of the loop: import sessions with the CLI
(e.g. from a session-end hook), then at task start let the agent call
`read_memory` (no args) to load the whole-memory digest, and `search_memory`
to pull the specific preferences, experiences, and facts a task needs. The MCP
server holds a single shared connection to `memory.duckdb`, so concurrent
tool calls do not contend for DuckDB's single-writer lock.

## Dreaming

Per-session extraction only merges new information into the handful of
memories it prefetches, so duplicates, contradictions, and cross-session
patterns accumulate between items that were never in the same extraction
context. A **dream cycle** is the global pass that cleans this up, in five
phases:

1. **Harvest** — discover finished sessions (Claude Code / OpenCode / Zed,
   inactive for at least the idle window, never imported) and run them through
   the regular import pipeline.
2. **Consolidate** — cluster near-duplicate items by embedding similarity and
   ask the model to merge each cluster. Contradictions are treated as the most
   valuable signal: conflicting memories become one item carrying the boundary
   insight ("retry works on the connection pool, not under an open
   transaction") instead of silently dropping a side.
3. **Reflect** — one pass over the whole store proposing a few higher-level
   items: repeated experiences promoted to a `skill`, the same fact recorded
   under several projects generalized to one global item.
4. **Synthesize skills** — a focused pass over the `experience`/`skill` items,
   distilling procedures that recur across sessions into reusable `skill` items
   (when to use, steps, prerequisites, failure modes). Write-only, like reflect.
5. **Refresh** — regenerate the `memory://memory` digest and record the run in
   `memory_dream_runs`.

Guardrails bound the blast radius of a misbehaving model: operations are
capped per cycle, consolidation may only delete items in the cluster it was
shown, reflection may not delete at all, and total deletions are limited to
a fraction of the store.

```bash
# Run one cycle now
codesearch memory dream
```

`codesearch serve` schedules dreaming automatically: a sweep every 15 minutes
imports freshly finished sessions, and a full cycle runs every 4 hours
(persisted across restarts via the last-run record). Configure it in the
`memory` section of `~/.codesearch/config.json`:

```jsonc
{
  "memory": {
    "dream_enabled": true,        // scheduled dreaming in serve mode
    "dream_interval_hours": 4,
    "session_idle_minutes": 60,   // when a session counts as finished
    "auto_import": true           // the 15-minute harvest sweep
  }
}
```

The management API exposes the same controls: `GET /api/memory/dream` returns
scheduler status plus the last run, and `POST /api/memory/dream` triggers a
cycle in the background.

## Update semantics

Updates use a rewrite-merge suited to a single-model pass: the extraction
prompt includes the current content of related existing items, and the model
must re-emit the **full rewritten content** under the same `(kind, name)` to
update an item. Contradicted or obsolete items are removed via the `delete`
list in the same response.

## Architecture

Following the ports & adapters layering:

- `src/domain/models/memory.rs` — `MemoryKind`, `MemoryItem`,
  `SessionTranscript`, `MemoryOperation`, `ImportedSession`, plus the
  virtual-filesystem `MemoryNode` / `NodeKind`.
- `src/application/interfaces/memory_repository.rs` — `MemoryRepository`
  port.
- `src/application/use_cases/memory_extraction.rs` (+ `_prompt.rs`) —
  extraction orchestration: prefetch → LLM call → parse/validate → apply.
- `src/application/use_cases/memory_summary.rs` — the L0/L1 layer:
  per-session node summarization and whole-memory digest regeneration.
- `src/application/use_cases/import_session.rs` — idempotence + session
  recording around extraction + summarization.
- `src/application/use_cases/memory_search.rs` — hybrid recall with RRF.
- `src/application/use_cases/memory_dream.rs` (+ `_prompt.rs`) — the dream
  cycle: harvest → similarity clustering → consolidation/reflection → digest
  refresh, with per-phase guardrails.
- `src/connector/adapter/management/dream.rs` — the serve-mode scheduler and
  the shared state behind `GET/POST /api/memory/dream`.
- `src/connector/adapter/claude_transcript.rs` — transcript parser.
- `src/connector/adapter/duckdb_memory_repository.rs` — the
  `memory.duckdb` adapter.

The LLM is reached through the existing `ChatClient` port
(`AnthropicClient` / `OpenAiChatClient`), and embeddings through the existing
`EmbeddingService` port, so every backend combination that works for code
search works for memory too.
