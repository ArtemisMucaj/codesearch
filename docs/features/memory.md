# Long-Term Memory

CodeSearch can import **finished assistant sessions** (e.g. Claude Code
transcripts) and distill them into durable, searchable memories: user
preferences, reusable experiences, procedural skills, and project facts. The
design follows OpenViking's session-memory architecture — declarative memory
kinds, an LLM extraction pass over the transcript with existing memories
prefetched for in-place merging, and per-field merge semantics — adapted to
CodeSearch's hexagonal layering and DuckDB storage.

## Storage

Memory lives in its **own DuckDB file**, `~/.codesearch/memory.duckdb`,
separate from the code index (`codesearch.duckdb`):

| Table | Contents |
|---|---|
| `memory_items` | One row per memory (`kind`, `name`, Markdown `content`, provenance, timestamps, update count). Unique per `(kind, name)`. |
| `memory_vectors` | `FLOAT[dims]` embedding per item for semantic search. |
| `memory_sessions` | Imported-session markers (idempotence + audit). |
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

## Recalling memories

```bash
# Hybrid search (semantic + keyword, fused with RRF)
codesearch memory search "how do we handle duckdb lock conflicts"

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

## Update semantics

OpenViking merges memory fields with per-field `merge_op`s (`patch`, `sum`,
`replace`, `immutable`). CodeSearch keeps the same *outcome* with a simpler
mechanism suited to a single-model pass: the extraction prompt includes the
current content of related existing items, and the model must re-emit the
**full rewritten content** under the same `(kind, name)` to update an item
(a rewrite-merge, equivalent to OpenViking's patch-by-rewrite path).
Contradicted or obsolete items are removed via the `delete` list in the same
response.

## Architecture

Following the ports & adapters layering:

- `src/domain/models/memory.rs` — `MemoryKind`, `MemoryItem`,
  `SessionTranscript`, `MemoryOperation`, `ImportedSession`.
- `src/application/interfaces/memory_repository.rs` — `MemoryRepository`
  port.
- `src/application/use_cases/memory_extraction.rs` (+ `_prompt.rs`) —
  extraction orchestration: prefetch → LLM call → parse/validate → apply.
- `src/application/use_cases/import_session.rs` — idempotence + session
  recording around extraction.
- `src/application/use_cases/memory_search.rs` — hybrid recall with RRF.
- `src/connector/adapter/claude_transcript.rs` — transcript parser.
- `src/connector/adapter/duckdb_memory_repository.rs` — the
  `memory.duckdb` adapter.

The LLM is reached through the existing `ChatClient` port
(`AnthropicClient` / `OpenAiChatClient`), and embeddings through the existing
`EmbeddingService` port, so every backend combination that works for code
search works for memory too.
