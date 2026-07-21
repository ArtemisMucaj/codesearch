# Memory System Design: An Append-Only Claim Graph with Consolidation

**Date:** 2026-07-21
**Status:** Proposed (revised after a codebase de-risking pass)

> **Relationship to the current implementation.** Today's memory system
> ([`docs/features/memory.md`](../features/memory.md)) uses *rewrite-merge*
> semantics: an update re-emits an item's full content under the same
> `(kind, name)` and bumps its `update_count`, mutating the row in place. This
> document proposes a different foundation — an append-only claim log with a
> typed relationship graph and a materialized read model — and treats the
> existing dream cycle as the consolidation pass over that log. It is a
> forward-looking design, not a description of what ships today.
>
> **What this revision cut, and why.** A pass over the actual codebase
> (transcript parser, extraction/dream use cases, the `ChatClient` backends,
> the DuckDB memory schema) showed three pieces of the original design rest on
> signals the pipeline does not have. They are excluded rather than faked:
>
> - **World-valid-time bi-temporality.** Transcripts carry only a per-message
>   *recording* timestamp — there is no independent signal for "when this became
>   true in the world." The two-timeline model collapsed to one in practice, so
>   the design now keeps a **single event timeline** (`recorded_at`) plus a
>   validity interval closed on supersession (`valid_to`). "What was true in the
>   world on date D" is demoted to a best-effort special case (only when an
>   explicit date is extracted from text), not a core query.
> - **`TextSpan` provenance.** The transcript is normalized and reassembled
>   (tool calls summarized, thinking/results dropped), so there is no stable
>   character offset back to a source. Provenance is now a coarse
>   **`(session_id, message_index)`** reference.
> - **`source_trust × confidence` arbitration.** Neither factor exists today —
>   `source` is a bare session id and extraction emits no confidence. Arbitration
>   is restated in terms of signals we can actually produce (see §7).
>
> The two assumptions that a code read *cannot* settle — whether atomic claims
> embed well enough for recall (§12), and whether entity resolution is good
> enough to anchor retrieval (§10.4) — are left in and explicitly marked as
> **spike-gated**: they must be validated on real data before the retrieval
> rewrite is committed.

---

## 0. TL;DR

Don't mutate memories. Treat memory as an **append-only claim log** where an
"update" is a *new* claim that closes out an old one, linked by a typed edge.
Layer a **typed relationship graph** on top, and serve reads from a
**materialized "current truth" projection** so you never pay graph-walk cost at
query time. Resolve the easy conflicts inline; defer the hard ones to an offline
**consolidation ("dreaming") pass** that runs a stronger model over the whole
graph to abstract, merge, re-score, prune, and rebuild the read model.

In one line: it's event sourcing for memory, with CQRS on the read side and a
periodic compaction/reflection job — the biological analogue of sleep.

---

## 1. Core principle: append-only, non-destructive

A memory is never edited in place and never deleted as part of normal operation.
Every new piece of information is a new **claim**. When new information replaces
old information, you don't overwrite — you write the new claim and add a
`supersedes` edge to the old one, which keeps its own validity intact *for the
window in which it was true*.

Why this matters:

- **Auditability** — you can always answer "what did I believe about X, and
  when did I come to believe it?" That question is impossible once you mutate.
- **Reversibility** — a bad extraction or a wrong supersession can be undone
  because the original is still there.
- **Supersession history** — "what did I record earlier about X, and when did
  the current version take over?" is answerable from the log, not just "what's
  current now?" (This is recording-order history, not independent world-time —
  see the revision note.)

This is the same reason event-sourced systems keep the event log instead of only
the current row: the log is truth, the current state is a derived view.

---

## 2. The false either/or

"Update in place" vs "keep everything and link with `supersedes`" is not a real
choice. The second *is* what a correct update looks like. But `supersedes` alone
is too blunt — collapsing every relationship into one keyword destroys exactly
the signal you need to resolve future conflicts. You need a small typed
vocabulary.

---

## 3. Relationship semantics

| Edge | Meaning | Both true at once? | How it resolves |
|------|---------|--------------------|-----------------|
| `supersedes` | Temporal replacement — old was true, new is true now | No (different windows) | Recency wins; old claim stays valid for its window |
| `contradicts` | Genuine conflict, no temporal ordering | No | Recency is a **bad** arbiter — use source + confidence, or flag |
| `refines` | Enrichment / specialization ("has a pet" → "has a dog named Rex") | Yes | Not a conflict; keep both, link parent↔child |
| `retracts` | Old claim was **never** true (bad extraction) | N/A | Mark original `retracted`, exclude from current view |
| `corroborates` | Independent source confirms an existing claim | Yes | Raise confidence, keep both for provenance |
| `relates_to` | Generic association discovered later | Yes | Navigational only |

The critical distinction is `supersedes` vs `contradicts`. If you flatten them,
a low-confidence recent claim will silently "supersede" a high-confidence
established fact. Keep them separate and arbitrate differently.

---

## 4. Data model

### 4.1 Claim (immutable, append-only)

```
Claim {
  id:              UUID
  subject:         EntityRef        # resolved, canonical entity
  predicate:       string           # small controlled-ish vocabulary
  object:          EntityRef | Literal
  statement:       string           # human-readable form, for retrieval/embedding
  project:         string | null    # project/namespace scope, or null = global

  # Temporal (single event timeline)
  recorded_at:     timestamp        # when the system recorded this claim
  valid_to:        timestamp | null # closed when a supersedes edge fires;
                                    #   null = still current
  valid_from:      timestamp        # defaults to recorded_at; only distinct when
                                    #   an explicit date is extracted from text

  # Trust
  source:          SourceRef        # (session_id, message_index) it came from
  source_kind:     enum { user_stated, assistant_inferred, derived }
  confidence:      float            # best-effort; model-emitted or heuristic

  # Lifecycle
  status:          enum { active, superseded, retracted, needs_resolution }
  derived:         bool             # true if produced by consolidation, not ingestion
  derived_from:    [Claim.id]       # source claims, if derived
}
```

There is one timeline: `recorded_at` is when the claim entered the log, and
`valid_to` is set to the recording time of the claim that supersedes it. That is
enough to answer "what did I record about X, and when did each version take
over" — the history question worth keeping. It is **not** a second, independent
world-valid timeline: the transcript has no signal for when a fact became true
in the world, so `valid_from` defaults to `recorded_at` and only differs when the
extractor lifts an explicit date out of the text. Treat world-time queries as a
best-effort extra, not a guarantee.

`project` mirrors the current `MemoryItem.project`: it is resolved at ingestion
by the existing shared resolver (git-remote-first, namespace-aware) so a claim
never surfaces as advice in an unrelated project. Entities (§4.2) stay global,
keyed by canonical name; the project scope lives on the claim.

### 4.2 Entity (resolved, canonical)

```
Entity {
  id:       UUID
  type:     string            # person, place, project, ...
  canonical_name: string
  aliases:  [string]          # "Alice", "my coworker Alice" → same node
  embedding: vector           # for fuzzy resolution at ingestion
}
```

Entity resolution ("Alice" == "my coworker Alice") happens at ingestion against
existing entities; ambiguous merges get deferred to consolidation.

### 4.3 Edge (typed, between claims)

```
Edge {
  from:       Claim.id
  to:         Claim.id
  type:       enum { supersedes, contradicts, refines, retracts, corroborates, relates_to }
  created_at: timestamp
  created_by: enum { ingestion, consolidation, manual }
  confidence: float
}
```

---

## 5. Layered architecture

1. **Claim log** — immutable, append-only. Ground truth and audit trail.
2. **Relationship graph** — typed edges between claims (and entity nodes).
3. **Materialized current-truth projection** — the read model you actually serve:
   the set of `active` claims with `valid_to = null`, entity-resolved and
   conflict-collapsed. Rebuilt by projection, never authored directly.

Layer 3 is CQRS: you don't walk the graph and filter validity windows on every
read — you maintain a projection and read from it in O(1)-ish. If you've dealt
with resolving deep relationship graphs at query time (e.g. ReBAC/Zanzibar-style
authz), this is the same "materialize instead of walk" tradeoff.

---

## 6. Online ingestion path (fast, cheap)

Runs on every new input. Keep the model's job **small and bounded**:

1. **Extract** candidate claims from the input (structured output).
2. **Resolve entities** against existing canonical entities.
3. **Retrieve prior state** — pull existing claims about the involved entities
   and *feed them into the prompt*. This is the single highest-leverage design
   choice: stateless extraction lets contradictions pile up next to the facts
   they should supersede; state-aware extraction lets the model mark a claim as
   an update instead of a duplicate.
4. **Decide** per candidate: new claim / `refines` existing / clear
   `supersedes` / **can't tell** → `needs_resolution`.
5. **Write** the claim (+ edge) to the log and update the projection.

Design rules:

- Keep the edge vocabulary small.
- Don't force the model to guess a hard edge type — ambiguity is a first-class
  `needs_resolution` state, not a coin flip. A flagged conflict you resolve later
  beats a confident wrong `supersede`.
- Reliable structured output is make-or-break; strong native schema adherence
  means far less downstream normalization.

**Model for this stage:** small, fast, non-thinking. On an 8 GB card, Granite
4.1 8B or Qwen3.5-9B at Q4 works. Extraction throughput matters more than depth
here.

---

## 7. Conflict resolution

- **Inline:** only the cheap, unambiguous cases (clear temporal supersession
  with high confidence).
- **Deferred:** everything ambiguous drops into the `needs_resolution` queue and
  is settled during consolidation, where you can afford a bigger model and full
  graph context.
- **Arbitration policy** (restated in terms of signals the pipeline actually
  produces — see the revision note at the top):
  - `supersedes` → recency wins.
  - `contradicts` → **`source_kind` precedence is the primary arbiter**
    (`user_stated` > `assistant_inferred` > `derived`); `confidence` is only a
    tiebreak within the same source kind. If the two are the same kind and
    within a confidence margin, keep both and surface the conflict rather than
    guessing.
  - Never let recency alone override a `user_stated` claim with a low-confidence
    inference.

  `confidence` is a soft, best-effort number (model-emitted when the backend is
  reliable enough, otherwise a heuristic from corroboration count and recency),
  so the policy leans on the discrete `source_kind` ordering first and treats
  `confidence` as advisory.

---

## 8. Consolidation — the "dreaming" pass

This is the interesting part. Biological memory doesn't just accumulate; during
sleep the brain **replays** recent episodes, **consolidates** them from fast
episodic storage into slower semantic storage, **abstracts** patterns across
them, and **prunes** weak traces. An agent memory system wants the same offline
phase — and it fits this architecture cleanly because the claim log is already
an event stream, and consolidation is just a scheduled job over it.

Conceptually it's **log compaction + projection rebuild + reflection**, run
asynchronously. (The "reflection" step of generative-agent systems — periodically
synthesizing higher-level observations from lower-level ones — is the same idea.)

### 8.1 What a dreaming pass does

1. **Abstraction (episodic → semantic).** Cluster many specific claims and
   derive higher-level ones. Five separate "worked late before the March
   release" episodes → one semantic claim "tends to work late around releases."
   Derived claims are tagged `derived = true`, carry `derived_from` provenance,
   and get **lower** confidence than primary observations.
2. **Resolve the `needs_resolution` queue** with full graph context and a
   stronger model — the conflicts too hard to call inline.
3. **Entity merge / dedup** — collapse duplicate entities that ingestion created
   separately.
4. **Importance re-scoring & decay** — raise salience of frequently-retrieved
   claims, decay stale ones. This is where confidence ages.
5. **Prune & tier** — move superseded / low-value claims to cold storage or
   replace a cluster with its summary.
6. **Discover cross-links** — add `relates_to` / `corroborates` edges that
   weren't visible at single-input ingestion time.
7. **Rebuild the materialized current-truth view.**

### 8.2 Where and when it runs

- **Offline / async**, decoupled from the ingestion path. Nothing here is on the
  latency-critical read or write path.
- **Bigger model, thinking mode on** — the reasoning-heavy work (abstraction,
  contradiction arbitration) is exactly where model quality pays off. This is
  the natural home for the "second tier": a small model online, a large model
  in the dream. On a mixed setup, the 8 GB box does ingestion; the high-memory
  machine runs the nightly pass with something like a 27B / 35B-A3B-class model.
- **Trigger:** nightly, on-idle, or on a "sleep pressure" threshold (enough new
  claims accumulated since the last pass).

### 8.3 Guardrails (so dreaming doesn't corrupt memory)

- **The immutable claim layer is sacrosanct.** Consolidation only *adds* derived
  claims and edges, updates lifecycle status, and rebuilds the projection. It
  never rewrites or deletes a primary claim.
- **Derived ≠ ground truth.** Every derived claim is provenance-linked and
  re-derivable, carries lower confidence, and is clearly flagged so it can never
  masquerade as a primary observation.
- **Hallucination is the real risk.** An LLM summarizing memories can invent
  facts. Mitigate with low confidence on derived claims, mandatory source
  citation, and optionally a verification step that checks each derived claim
  against its sources before it's admitted.
- **Idempotency / convergence.** Repeated passes must not drift or endlessly
  rewrite. Don't re-abstract already-stable clusters; make the pass converge to
  a fixpoint on unchanged input.

---

## 9. Forgetting & tiering

Append-only does **not** mean "keep everything hot forever" — that's a
retrieval-noise and storage liability. It means forgetting is *deliberate*, done
during consolidation, not destructive mutation at write time:

- **Hot:** the materialized current-truth view.
- **Warm:** recent primary claims.
- **Cold:** superseded / retracted / low-salience claims, moved out of the
  serving path (retrievable for audit, not for normal reads).
- **Summarized:** dense clusters replaced by a derived summary claim, originals
  cold-stored.

Retention windows govern the transitions.

---

## 10. Retrieval

Retrieval is where the design's central tension surfaces: the store is highly
**structured** (typed edges, supersession chains, resolved entities) but queries
arrive **fuzzy** (natural language, vague reference). The resolution is not to
pick a paradigm but to split the work: semantics and structure do different jobs.

### 10.1 Two principles

- **Fuzzy search is an *entry-point finder*, not the retrieval itself.** You do
  not semantic-search the graph; you semantic-search to find your way *into* it,
  then let structure do the heavy lifting.
- **Serve from the materialized current-truth view, not the raw log.** The
  projection is already conflict-collapsed and active-only, so reads don't redo
  conflict resolution or validity filtering. This is the read side of the CQRS
  split — you read a resolved claim rather than re-deriving it per query. Only
  history / audit queries (what superseded what, and when it was recorded) fall
  through to the full claim log.

### 10.2 Pipeline

1. **Entry-point resolution (fuzzy -> anchors).** Resolve the query to *entities*
   (via entity embeddings + the alias table — the strongest anchor) and to
   *candidate claims* via a **hybrid** vector + BM25 search over claim
   statements. Hybrid is not optional: proper nouns, IDs, and exact tokens need
   lexical recall; concepts need semantic. Vector-only silently drops the query
   that hinges on a name.
2. **Graph expansion.** From the anchors, walk typed edges 1-2 hops — pull
   `refines` children, `corroborates`, `relates_to` neighbors. Follow
   `supersedes` / `contradicts` edges to *understand* status, not to surface the
   superseded claims themselves.
3. **Filter by status + validity.** Drop `superseded` / `retracted` unless the
   query is a history query; when the query carries an explicit date, apply the
   `recorded_at` / `valid_to` window. This is where the single-timeline validity
   model is spent.
4. **Rank.** Blend semantic similarity, recency, confidence, and the **salience
   score the consolidation pass already computed** — reuse it, don't recompute
   importance at query time. A cross-encoder rerank on the shortlist is worth it.
5. **Assemble.** For lookups, return the ranked subgraph. For synthesis queries
   ("what have I learned about X"), run a reflect step over the retrieved claims
   — the same machinery as consolidation, at query time.

### 10.3 Query routing

Not every query wants the full pipeline. Cheaply classify and route:

- **Semantic recall** ("what do I know about...") -> full pipeline above.
- **Precise lookup** ("what's the current value of setting X") -> structured
  query against the projection; skip vector search.
- **History / audit** ("what did I previously believe about X, and when was it
  replaced") -> structured query against the log over `recorded_at` / `valid_to`
  and the `supersedes` chain; skip vector search.
- **Multi-hop relational** ("E's project's owner's timezone") -> entity anchor +
  deeper bounded traversal.
- **Aggregation / synthesis** -> retrieve broadly, then reflect.

### 10.4 Stance

Prefer **graph-anchored** retrieval over vector-first GraphRAG. Vector-first
tends to return semantically-similar-but-stale-or-contradicted claims — exactly
what the supersession model exists to avoid. Use fuzzy search only to find entry
nodes and let structure decide the rest. It plays to what the graph gives you
instead of fighting it. Mature temporal-memory systems converge on the same
shape: one store carrying vector, lexical (BM25), and graph indexes together,
rather than a vector DB bolted beside a graph DB.

> **Spike-gated.** This stance *assumes* entity resolution is good enough to
> anchor on, and that atomic claims embed well enough for the hybrid entry-point
> leg to find them. Neither can be settled by reading the codebase — there is no
> entity-resolution infrastructure today, and atomic claims are known to embed
> noisily (§12). Both must be validated on real data before this retrieval path
> is built; if resolution is mushy or claim recall is poor, graph-anchored loses
> to the simple hybrid item search that already ships, and the stance should be
> revisited.

---

## 11. Model selection summary

| Stage | Profile needed | Runs where | Candidate |
|-------|----------------|------------|-----------|
| Extraction (online) | Reliable structured output, fast, non-thinking | 8 GB card | Granite 4.1 8B / Qwen3.5-9B @ Q4 |
| Conflict arbitration (deferred) | Reasoning, thinking mode | High-mem machine | 27B / 35B-A3B class |
| Consolidation / dreaming | Strong reasoning + abstraction, batch | High-mem machine, offline | 27B / 35B-A3B class |

Two-tier only because the workloads genuinely differ — not for its own sake. A
single mid-size model can do all three if you'd rather keep it simple.

> **Structured-output caveat (from the codebase).** The online extraction path
> gets real schema enforcement only on the OpenAI-compatible backend, which
> sends `response_format: json_schema` (strict) and degrades to free-form when
> the server can't grammar-constrain. The Anthropic backend — the natural home
> for the deferred arbitration / dreaming tier — implements no structured output
> at all and relies entirely on the tolerant free-form parser. So: keep the
> claim-extraction schema **flat** (deep nesting raises the fallback rate even on
> OpenAI), and either accept free-form parsing for the dreaming tier or add
> tool-use-based structured output to the Anthropic client as net-new work.

---

## 12. Open questions & honest tradeoffs

The two **spike-gated** items below (claim embedding, entity resolution) are the
ones that decide whether this whole approach beats the simple hybrid item search
that already ships. They are empirical — a code read can't settle them — so they
must be validated on real data *before* the retrieval rewrite is committed.
Everything the de-risking pass could settle from the codebase has already been
folded into the body of the design (see the revision note at the top): the
temporal model, provenance shape, arbitration signals, project scoping, delete
semantics, and write atomicity.

- **[SPIKE] Atomic claims embed poorly.** Short structured statements have thin
  "aboutness" and embed noisily, so pure vector recall over claims
  underperforms. Options to prototype: embed the statement plus light
  entity/parent context (e.g. a claim together with its `refines` parent), or
  embed at the entity/topic-cluster level and treat claims as the fine-grained
  payload hydrated after anchoring. Chunk granularity is a genuine open problem.
  Measure recall on real sessions before building the entry-point leg.
- **[SPIKE] Entity resolution is the linchpin and is greenfield.** There is no
  entity-resolution infrastructure in the codebase today. Graph-anchored
  retrieval is only as good as resolution; if "Alice" / "my coworker Alice"
  don't collapse reliably (embedding + alias table over the existing
  `EmbeddingService`), the anchoring stance fails. Validate resolution quality
  on the entities real sessions produce before committing §10.
- **How much to trust derived claims** at retrieval time vs. always deferring to
  primaries. Start conservative: primaries win, derived claims only fill gaps.
- **Consolidation cadence** — too frequent wastes compute and risks drift; too
  rare lets the `needs_resolution` queue and duplicates rot the current view.
- **Convergence** — needs real attention; an abstraction pass that isn't a
  fixpoint on stable input will churn.
- **Cost** — the dreaming pass is the expensive part. It's also the part you can
  run whenever the machine is idle, which is what makes the small-online /
  large-offline split practical.
- **Atomic claims embed poorly.** Short structured statements have thin
  "aboutness" and embed noisily, so pure vector recall over claims
  underperforms. Options to prototype: embed the statement plus light
  entity/parent context (e.g. a claim together with its `refines` parent), or
  embed at the entity/topic-cluster level and treat claims as the fine-grained
  payload hydrated after anchoring. Chunk granularity is a genuine open problem.
- **Graph-first vs vector-first is a real fork.** Vector-first (GraphRAG-style:
  embed everything, retrieve broadly, enrich with graph) is simpler but returns
  stale/contradicted-but-similar claims. Graph-anchored (fuzzy search only for
  entry points, structure decides the rest) plays to strong entity resolution
  but leans harder on that resolution being good. The recommendation here is
  graph-anchored, but it's worth validating against your own query mix.

---

*Design summary: an append-only claim graph on a single event timeline, with
typed relationships, a materialized current-truth read model, deferred conflict
resolution, and an offline consolidation pass that abstracts, merges, prunes, and
rebuilds — memory as an event log, with sleep. Two assumptions (claim embedding
and entity resolution) remain spike-gated before the retrieval path is built.*
