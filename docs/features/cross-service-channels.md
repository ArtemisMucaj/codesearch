# Cross-Service Channel Linking — Design Plan

**Status:** Draft / design only — no implementation yet
**Scope:** Detect and surface inter-repository connections through messaging and
transport channels (Kafka, HTTP, MQTT, AMQP, gRPC) when multiple projects are
indexed in the same namespace.

---

## Table of Contents

1. [Problem Statement](#problem-statement)
2. [Core Idea: Channels as Rendezvous Points](#core-idea-channels-as-rendezvous-points)
3. [Phase 1 — End-to-End Skeleton: Extract → Store → Match → List](#phase-1--end-to-end-skeleton-extract--store--match--list)
4. [Phase 2 — Graph Integration: Leiden, `uses`, Features, Impact](#phase-2--graph-integration-leiden-uses-features-impact)
5. [Phase 3 — Recall: Indirection Resolution, HTTP Hosts, LLM Fallback](#phase-3--recall-indirection-resolution-http-hosts-llm-fallback)
6. [Cross-Cutting Concerns](#cross-cutting-concerns)
7. [Risks & Mitigations](#risks--mitigations)
8. [Commit Decomposition](#commit-decomposition)

---

## Problem Statement

Everything cross-repo in codesearch flows through one mechanism today: **shared
symbol names**. `FileRelationshipUseCase::build_graph`
(`src/application/use_cases/file_relationship.rs`) builds a `symbol_name → file`
map across the namespace's repositories and emits an edge only when a
`symbol_references` row's callee resolves to a file in that map. Leiden
clustering (`cluster_detection.rs`), `uses --cross-repo`, impact analysis, and
execution features all sit on top of that graph.

Messaging breaks this contract by design:

```text
repo-A (producer):   producer.send("orders.created", payload)
repo-B (consumer):   @KafkaListener(topics = "orders.created")
```

There is **no symbol in common** — only a string literal. The same is true for
HTTP (`reqwest::get("http://orders-svc/api/orders")` vs
`.route("/api/orders", …)`) and MQTT topics. Two indexed services in a
namespace therefore look like disconnected islands: each gets its own Leiden
communities, and the consumer's handler shows up as an orphan entry point in
execution features.

A second structural fact shapes the design: **tree-sitter
(`ParserService::parse_file`) only produces chunks; all `SymbolReference`s come
from the SCIP importer** (the `Scip` trait hook in
`src/application/use_cases/index_repository.rs`). Channel extraction cannot
piggyback on an existing tree-sitter reference extractor — it needs its own
extraction pass.

## Core Idea: Channels as Rendezvous Points

Stop trying to link symbols. Instead, extract **communication endpoints**
(producer/consumer call sites with their channel identifier) and join them on
the **channel** — topic name, route template, queue name:

```text
repo-A: OrderService::checkout ──publishes──▶ [kafka: orders.created] ◀──consumes── repo-B: OrderConsumer::handle
```

The join produces **virtual cross-repo edges** that merge into the existing
`FileGraph`, from which every downstream feature (clusters, features, impact,
`uses`) already reads.

Three phases, each independently shippable:

| Phase | Deliverable | Depends on |
|---|---|---|
| 1 | Endpoint extraction, storage, matching, `codesearch channels` CLI | — |
| 2 | Channel edges in Leiden / `uses` / execution features / impact | Phase 1 |
| 3 | Constant propagation, config-file resolution, HTTP host mapping, gRPC, LLM fallback | Phase 1 (2 optional) |

---

## Phase 1 — End-to-End Skeleton: Extract → Store → Match → List

**Goal:** two repos indexed in one namespace; `codesearch channels` prints
matched producer→consumer pairs and dangling endpoints. Nothing touches
clustering yet — this phase proves extraction quality before anything
downstream depends on it.

### 1.1 Domain model

New file `src/domain/models/channel_endpoint.rs` (pure values, `serde` only,
consistent with domain-layer rules):

```rust
pub struct ChannelEndpoint {
    id: String,
    repository_id: String,
    file_path: String,
    /// Function containing the call site — resolved by the same AST walk
    /// that fills `parent_symbol` on chunks. Optional but load-bearing:
    /// it is what lets impact analysis and execution features attach
    /// channels to call-graph nodes in phase 2.
    enclosing_symbol: Option<String>,
    line: u32,
    protocol: Protocol,                // Kafka | Http | Mqtt | Amqp | Grpc
    role: ChannelRole,                 // Producer | Consumer (HTTP: Client | Server)
    channel_raw: String,               // "orders.created", "/users/{id}"
    channel_normalized: String,        // template-normalized (see §1.5)
    host: Option<String>,              // HTTP only; unused until phase 3
    is_pattern: bool,                  // wildcard / regex subscription
    confidence: f32,
    source: EndpointSource,            // TreeSitter | Config | Llm
}
```

Derived match result (never persisted — see §1.5):

```rust
pub struct ChannelEdge {
    producer: ChannelEndpoint,
    consumer: ChannelEndpoint,
    weight: usize,      // distinct call sites collapsed per (file-pair, channel)
    confidence: f32,    // min(producer.confidence, consumer.confidence)
}
```

`Protocol`, `ChannelRole`, `EndpointSource` are enums with `as_str`/`parse`
round-trips, mirroring `ReferenceKind` in `symbol_reference.rs`.

### 1.2 Extraction port + adapter

New port `src/application/interfaces/channel_extractor.rs`:

```rust
#[async_trait]
pub trait ChannelExtractor: Send + Sync {
    async fn extract(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
    ) -> Result<Vec<ChannelEndpoint>, DomainError>;

    fn supports_language(&self, language: Language) -> bool;
}
```

Adapter at `src/connector/adapter/tree_sitter_channels/` — a sibling of
`treesitter_parser.rs`, which is already at its ~300-line-per-file guidance
ceiling. Internally it is a **detector registry**: each detector is data, not
code —

```rust
struct Detector {
    language: Language,
    protocol: Protocol,
    role: ChannelRole,
    query: &'static str,            // tree-sitter S-expression
    channel_capture: &'static str,  // capture holding the channel argument
    confidence: f32,
}
```

The engine runs every detector registered for the file's language; for each
match it extracts the string literal, normalizes it, and walks up the AST to
find the enclosing function (same walk that sets `parent_symbol` today). When
the channel argument is an **identifier instead of a literal**, the endpoint is
still recorded — with the identifier stored in `channel_raw`, a distinct
"unresolved" marker, and excluded from matching until phase 3 resolves it.
Recording rather than dropping is deliberate: unresolved endpoints power the
phase-3 recall work and make the phase-1 unmatched report honest.

Starter detector set — deliberately narrow, high precision:

| Detector | Language | Producer / client shape | Consumer / server shape |
|---|---|---|---|
| kafka-python | Python | `producer.send("t", …)`, `KafkaProducer` | `KafkaConsumer("t")`, `.subscribe([...])` |
| spring-kafka | Java/Kotlin | `kafkaTemplate.send("t", …)` | `@KafkaListener(topics = "t")` |
| axum | Rust | — | `.route("/p", get(h))` |
| reqwest | Rust | `client.get("…")`, `reqwest::get("…")` | — |
| express | JS/TS | — | `app.get('/p', h)`, `router.post(…)` |
| axios / fetch | JS/TS | `axios.get('…')`, `fetch('…')` | — |
| paho / mqtt.js | Python/JS | `client.publish("a/b")` | `client.subscribe("a/+")` |

Precision note: matching `producer.send(...)` by method name will occasionally
fire on non-Kafka objects. Acceptable in phase 1 because **a false producer
endpoint only becomes a false edge if a matching consumer exists on the same
channel string** — the join is itself a strong filter. Confidence scoring
covers the rest.

Adding a framework later = one registry entry + one fixture test.

### 1.3 Indexing pipeline hook

`parse_only` in `index_repository.rs` already has file content + language in
hand. Changes:

- Extend `ParseOnlyResult` with `endpoints: Vec<ChannelEndpoint>` and call the
  extractor right after `parse_file`.
- Cost: one extra tree-sitter parse per file. Acceptable for phase 1; if
  profiling says otherwise, refactor `ParserService` to return the parsed tree
  for reuse — do **not** pre-pay that complexity.
- Incremental indexing gets the same lifecycle plumbing references have: on
  modified/deleted files call `delete_by_file_path`, on repo delete call
  `delete_by_repository` — mirroring the exact call sites where
  `call_graph_use_case.delete_by_file` runs today.

### 1.4 Storage

`DuckdbChannelEndpointRepository` at
`src/connector/adapter/duckdb_channel_endpoint_repository.rs`, modeled on
`duckdb_call_graph_repository.rs` (namespace-scoped schema, idempotent DDL,
`with_connection` / `with_connection_no_init` constructors):

```sql
CREATE TABLE IF NOT EXISTS channel_endpoints (
    id VARCHAR PRIMARY KEY,
    repository_id VARCHAR NOT NULL,
    file_path VARCHAR NOT NULL,
    enclosing_symbol VARCHAR,
    line INTEGER,
    protocol VARCHAR NOT NULL,
    role VARCHAR NOT NULL,
    channel_raw VARCHAR NOT NULL,
    channel_normalized VARCHAR NOT NULL,
    host VARCHAR,
    is_pattern BOOLEAN DEFAULT FALSE,
    resolved BOOLEAN DEFAULT TRUE,
    confidence FLOAT NOT NULL,
    source VARCHAR NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chan_norm ON channel_endpoints(protocol, channel_normalized);
CREATE INDEX IF NOT EXISTS idx_chan_repo ON channel_endpoints(repository_id);
CREATE INDEX IF NOT EXISTS idx_chan_file ON channel_endpoints(file_path, repository_id);
```

Port trait `ChannelEndpointRepository` in `src/application/interfaces/`
follows the `CallGraphRepository` shape:

```rust
#[async_trait]
pub trait ChannelEndpointRepository: Send + Sync {
    async fn save_batch(&self, endpoints: &[ChannelEndpoint]) -> Result<(), DomainError>;
    async fn find_by_repository(&self, repository_id: &str) -> Result<Vec<ChannelEndpoint>, DomainError>;
    async fn find_by_protocol(&self, protocol: Protocol) -> Result<Vec<ChannelEndpoint>, DomainError>;
    async fn delete_by_file_path(&self, repository_id: &str, file_path: &str) -> Result<u64, DomainError>;
    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;
    async fn get_stats(&self, repository_id: &str) -> Result<ChannelStats, DomainError>;
}
```

An `InMemoryChannelEndpointRepository` twin serves tests, same as the vector
store.

### 1.5 Matching — `ChannelLinkUseCase`

New use case `src/application/use_cases/channel_link.rs`.

**Edges are computed at query time, never materialized.** Endpoint counts are
small (hundreds, not millions), matching is cheap, and query-time matching
means re-indexing one repo never leaves stale edges pointing at it — the same
reason `FileRelationshipUseCase` builds its graph on demand.

Algorithm:

1. Load endpoints for the target repos (or whole namespace), grouped by
   protocol; drop unresolved ones.
2. **Exact pass** — hash-join producers ↔ consumers on `channel_normalized`
   where roles oppose.
3. **Pattern pass** (endpoints with `is_pattern` only):
   - MQTT: segment-wise wildcard match (`+` = exactly one segment, `#` =
     any suffix).
   - Kafka regex subscriptions (`Pattern.compile("orders.*")`): `regex` crate
     against concrete producer topics.
4. **HTTP template pass** — normalization at extraction time rewrites
   `{id}` / `:id` / `<id>` to a canonical `{}` segment; matching walks
   segments where `{}` on either side matches any single concrete segment
   (`/users/123` client ↔ `/users/{}` server).
5. Edge weight = distinct producer call sites × distinct consumer sites,
   collapsed per (file-pair, channel). Edge confidence =
   `min(producer.confidence, consumer.confidence)`.

**Fan-out guardrail (built now, not later):** a channel like `/health` matched
by 40 client sites across every repo is noise. Channels whose edge count
exceeds a threshold get flagged in output, and the CLI supports
`--exclude-channel <glob>`.

Normalizer and matcher are pure functions — trivially table-testable.

### 1.6 Surfacing

- **CLI**: `codesearch channels [--repo …] [--protocol …] [--unmatched]
  [--min-confidence …] [--exclude-channel …]` in `src/cli/mod.rs`, routed via
  a new `src/connector/api/controller/channels_controller.rs`. Output: matched
  edges grouped by channel, then **dangling producers/consumers**. The
  unmatched list is the extraction-quality feedback loop and the debugging
  answer to "why isn't my link showing".
- **MCP**: a `channels` tool alongside the existing ones in
  `src/connector/adapter/mcp/tools.rs`, so agents can ask "what connects these
  services".
- **DI**: wire adapter → port → use case in `src/connector/api/container.rs`,
  in the same block where `CallGraphUseCase` is built.

### 1.7 Tests

`tests/fixtures/messaging/` with two mini-repos (< 100 lines each, per fixture
guidance):

- `orders-service` — Python: Kafka producer (`orders.created`) + Flask route
  (`/api/orders/<id>`).
- `notification-service` — JS: Kafka consumer on `orders.created` + axios
  client calling `/api/orders/123`.

Integration test (new file `tests/channel_link_tests.rs`, `ContainerConfig`
with `memory_storage: true`, `mock_embeddings: true`):

- Index both repos into one namespace; assert the Kafka edge and the HTTP edge
  both return with correct endpoints, roles, and confidence.
- A bogus topic stays in the unmatched list.
- Re-index after modifying the producer file; assert no duplicate endpoints
  (incremental lifecycle).

Unit tests: MQTT wildcard matcher, HTTP route template normalizer/matcher,
Kafka regex pass.

### Phase 1 exit criteria

- Fixture namespace shows both cross-repo edges; unmatched list empty for
  fixtures.
- On a real two-repo namespace, `codesearch channels --unmatched` produces an
  actionable report.
- Zero behavior change to every existing command.

---

## Phase 2 — Graph Integration: Leiden, `uses`, Features, Impact

**Goal:** channel edges flow into every consumer of the file graph, without
corrupting single-repo analysis quality.

### 2.1 Merge into `FileGraph`

`FileRelationshipUseCase::build_graph` gains an optional
`Arc<ChannelLinkUseCase>`. After step 3 (symbol-reference aggregation) it
appends channel edges as `FileEdge`s:

- `reference_kinds = ["channel:kafka"]`, `["channel:http"]`, etc.
- Channel names go into the existing `symbols: Vec<String>` field — it already
  means "what connects these files".
- Cross-repo channel edges are emitted **only when `include_cross_repo` is
  true**, matching existing symbol-edge behavior. Same-repo channel matches
  (a service consuming its own topic) are still emitted as normal edges.
- `FileEdge` gains `confidence: Option<f32>` — `None` for AST edges
  (= certain), `Some(c)` for channel edges — so downstream consumers can
  threshold.

Determinism guard: the Leiden doc promises stable output from sorted edge
lists. Channel edges are appended **before** the existing deterministic sort in
`build_graph`, so ordering stays stable.

### 2.2 Leiden weights (`cluster_detection.rs`)

Extend `kind_weight`:

```rust
"channel:grpc"                                   => 0.6,  // typed contract — near type_reference
"channel:kafka" | "channel:amqp" | "channel:mqtt" => 0.35,
"channel:http"                                   => 0.3,  // below IMPORT_WEIGHT (0.5)
```

Rationale for going low: within one namespace each service should **remain its
own community** — the deliverable is *labeled edges between communities*, not
one merged mega-cluster. But the weight becomes a real knob
(`--channel-weight`, default as above; `0` = ignore channels entirely) because
the opposite mode is legitimate: "cluster my 12 repos into messaging
subsystems" wants channel weight ≥ call weight.

`composite_weight` needs no change: it multiplies base weight (call-site
count, which channel edges carry) by mean kind weight.

### 2.3 Cluster & visualization output

- `ClusterGraph` inter-cluster edges and `GraphEdge` (in
  `src/domain/models/graph_view.rs`) gain an optional `via: Vec<String>`
  (e.g. `["kafka:orders.created"]`).
- `clusters_controller`, `visualize_controller`, and the TUI graph views render
  it as edge labels. This is the headline UX:
  **community A → community B via `kafka:orders.created`**.

### 2.4 Execution features (`execution_features.rs`)

Two changes:

1. **Labeling.** `find_entry_points` already finds consumer handlers (no
   callers). Join entry points against consumer endpoints on
   `(repository_id, file_path, enclosing_symbol)`; on hit, `ExecutionFeature`
   gets `trigger: Option<ChannelTrigger { protocol, channel }>`. "Orphan
   function" becomes "Kafka feature fed by `orders.created`".
2. **Criticality.** New signal: cross-repo producer fan-in (how many producer
   sites in *other* repos feed this entry point). Weights must keep summing to
   1.0 — rebalance to approximately:

   | Signal | Old | New |
   |---|---|---|
   | `WEIGHT_FILE_SPREAD` | 0.35 | 0.30 |
   | `WEIGHT_EXTERNAL_CALLS` | 0.25 | 0.20 |
   | `WEIGHT_TEST_COVERAGE_GAP` | 0.25 | 0.25 |
   | `WEIGHT_DEPTH` | 0.15 | 0.10 |
   | `WEIGHT_CHANNEL_FANIN` (new) | — | 0.15 |

   Subtlety to fix while here: `TEST_COVERAGE_GAP_SCORE` assumes entry points
   have no known callers *by definition*. A channel-fed entry point now **does**
   have known upstream producers, so the gap score should not apply at full
   strength to matched consumers.

### 2.5 Impact analysis (`impact_analysis.rs`)

Opt-in channel hop behind `--cross-channel`:

- When the BFS visits symbol `S`, also look up producer endpoints with
  `enclosing_symbol == S`. For each matched consumer, enqueue the consumer's
  handler symbol (other repo) with an `ImpactNode` edge kind
  `channel:kafka(orders.created)` and confidence multiplied along the path.
- `--min-confidence` prunes (default ~0.5).
- A channel hop counts as one BFS level; confidence does the pruning (simpler
  than a separate depth cost, and the confidence field already exists).
- Symmetrically, `symbol_context` callers-direction can traverse
  consumer → producer ("who triggers this handler?").

Opt-in matters: channel edges are inferred, and silently inflating blast
radius would erode trust in numbers people already rely on.

### 2.6 Search integration

Store channel names as chunk-adjacent metadata so keyword search resolves
"who publishes orders.created". Cheapest viable form: the channels controller
supports lookup by channel substring; full BM25 integration can wait for
demand.

### 2.7 Tests

Extend the phase-1 fixture namespace:

- Two Leiden communities remain distinct at default channel weight, with a
  labeled inter-community edge (`via` populated).
- `impact --cross-channel` on the producer function reaches the consumer's
  forward chain; without the flag it does not.
- Consumer entry point's feature carries the Kafka trigger and a nonzero
  channel fan-in component.
- **Regression guard:** on a namespace with zero endpoints, `clusters`,
  `uses`, `features`, `impact` outputs are byte-identical to pre-phase-2.

### Phase 2 exit criteria

- `clusters`, `uses`, `features`, `impact` all show channel awareness on the
  fixtures.
- Regression guard passes (no endpoints ⇒ identical output).

---

## Phase 3 — Recall: Indirection Resolution, HTTP Hosts, LLM Fallback

**Goal:** close the gap between "channels written as inline literals" (phases
1–2) and reality, where names live in constants and config. Every sub-item is
independent and separately mergeable.

> **Sequencing note:** validate phase-1 recall on a real namespace *before*
> starting phase 2. If literal-only recall is embarrassingly low for the
> stack in use, pull §3.1 (constant propagation) forward into "phase 1.5" —
> it is self-contained and does not depend on anything in phase 2.

### 3.1 Constant propagation (biggest recall win, cheapest)

The unresolved endpoints recorded in phase 1 (identifier instead of literal)
get a post-indexing resolution pass, per repo:

1. **File-local pass.** The extractor additionally runs a per-language
   "string constant definition" query (`const ORDERS_TOPIC = "…"`,
   `static TOPIC: &str = "…"`, module-level `ORDERS_TOPIC = "…"`, Java
   `static final`). Build `(file, name) → value` and resolve locally first.
2. **Repo-global pass.** For still-unresolved identifiers, follow the existing
   `Import` symbol references (the `import_alias` machinery on
   `SymbolReference` already handles renaming) to the defining file, then look
   up that file's constant table.
3. **Ambiguity.** Multiple candidate values (reassignment, per-env branches)
   ⇒ emit one endpoint per candidate at reduced confidence rather than
   guessing.

Entirely static and deterministic — no dataflow analysis, just "identifier →
const string ≤ 2 hops away", which empirically covers most real code.

### 3.2 Config-file resolution

- **New extraction targets:** YAML / TOML / properties / `.env` / JSON files
  are parsed (serde-based, not tree-sitter) into a namespace-scoped table:

  ```sql
  CREATE TABLE IF NOT EXISTS config_entries (
      repository_id VARCHAR, file_path VARCHAR,
      key VARCHAR, value VARCHAR      -- keys flattened: kafka.topics.orders
  );
  ```

- **New detector shapes** producing endpoints with a config-**key** reference
  instead of a value: `config.get("kafka.orders_topic")`, Spring
  `@Value("${kafka.orders_topic}")` / `@KafkaListener(topics = "${…}")`,
  `os.environ["ORDERS_TOPIC"]`, `process.env.ORDERS_TOPIC`.
- **Resolution** joins key → `config_entries`. Profile ambiguity
  (`application.yml` vs `application-prod.yml`): prefer the default/base file,
  emit alternates at lower confidence.
- Env vars appearing in no config file stay unresolved but **named** — the
  unmatched report shows `kafka consumer ← env:ORDERS_TOPIC (unresolved)`,
  which a human can act on.

### 3.3 HTTP host → repository mapping

Client URLs carry a host (`http://orders-svc/api/orders`); mapping host → repo
needs external knowledge. Three sources, best wins:

1. **Manual map** in the existing global `namespace_config` table:
   `codesearch namespace map-service orders-svc <repo-id>`. Ground truth, zero
   magic.
2. **Manifest inference:** when an indexed repo contains
   `docker-compose.yml` / k8s manifests, map service name → image/build
   context → repo. These files ride the §3.2 config pipeline.
3. **Name heuristic:** normalized host (`orders-svc`, `orders_service`) ≈
   normalized repo name — lowest confidence, used only when unambiguous within
   the namespace.

Matching then becomes: host known **and** mapped ⇒ require repo match (which
properly kills the `/health` false-positive class); host unknown ⇒
path-template-only match, confidence penalized when several repos serve the
same template.

### 3.4 gRPC / protobuf

- Parse `.proto` files (config pipeline again) → `package.Service/Method`
  channels.
- Servers = implementations of generated stubs, detectable via existing
  `Implementation` symbol references against generated names; clients = stub
  method calls.
- Highest-confidence protocol of all — the channel is a typed contract — hence
  its 0.6 Leiden weight in phase 2.

### 3.5 LLM fallback (last, optional, off by default)

For endpoints still unresolved after §3.1–3.3:

- Batch call sites (snippet + enclosing chunk content, already in the vector
  store) through the existing `ChatClient` port with a structured prompt:
  "what channel does this publish/consume, or UNKNOWN".
- Results: `source = Llm`, confidence from the response, hard-capped below all
  static sources.
- Gated behind `codesearch channels resolve --llm` — it needs network, and the
  no-network-in-tests rule means CI exercises it mock-only.

Deliberately last: every static improvement shrinks both the LLM bill and the
trust problem.

### 3.6 Tests

Fixtures grow by one repo each for: topic behind a constant, topic behind
`application.yml`, a compose file mapping `orders-svc`, a `.proto` contract.
Assert each resolution path independently, and that profile-ambiguous config
yields multiple lower-confidence candidates.

### Phase 3 exit criteria

- Fixture recall reaches 100% with correct confidence ordering
  (literal > constant > config > heuristic host > LLM).
- On a real namespace, the unmatched report visibly shrinks between phase-1
  and phase-3 builds.

---

## Cross-Cutting Concerns

### Architecture placement (DDD / Ports & Adapters)

| Layer | Additions |
|---|---|
| Domain | `ChannelEndpoint`, `Protocol`, `ChannelRole`, `EndpointSource`, `ChannelEdge`, `ChannelTrigger` |
| Application | `ChannelExtractor` + `ChannelEndpointRepository` ports; `ChannelLinkUseCase`; extensions to `FileRelationshipUseCase`, `ExecutionFeaturesUseCase`, `ImpactAnalysisUseCase` |
| Connector | `tree_sitter_channels/` detector registry; `DuckdbChannelEndpointRepository`; config-file parsers (phase 3); `channels_controller`; container + router wiring; MCP tool |
| CLI | `channels` command; `--cross-channel`, `--channel-weight`, `--min-confidence` flags |

All framework-specific knowledge stays quarantined in the connector layer.

### Schema evolution

Additive at every step — new tables (`channel_endpoints`, `config_entries`),
new optional fields on `FileEdge` / `ExecutionFeature` / `ImpactNode` /
`GraphEdge`. No migration of existing namespaces; re-indexing populates
endpoints.

### Performance

- Extraction: one extra tree-sitter parse per file (phase 1); shareable parse
  tree is the known optimization if profiling demands it.
- Matching: in-memory hash joins over hundreds of endpoints at query time;
  pattern passes are O(producers × pattern-consumers) per protocol, bounded.
- No new indexes on hot write paths beyond the three `channel_endpoints`
  indexes.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Recall bounded by indirection (literal-only misses config-driven channels) | Phase 3 exists for exactly this; unresolved endpoints are stored and *named* in the unmatched report, so misses are visible, not silent. Pull §3.1 forward if phase-1 recall disappoints. |
| False positives on generic strings (`/health`, shared prefixes) | Role opposition required; fan-out cap warning + `--exclude-channel`; confidence thresholds; host-required matching once §3.3 lands. |
| Channel edges corrupting single-repo analysis | Low default Leiden weights; `--channel-weight 0` escape hatch; impact traversal strictly opt-in; regression guard test (no endpoints ⇒ identical output). |
| Detector maintenance as framework APIs churn | Detectors are registry data (query + capture + confidence), not code; one entry + one fixture test per framework. |
| Non-determinism in Leiden output | Channel edges enter the edge list before the existing deterministic sort; fixed-seed refinement unchanged. |
| LLM cost / trust | Last resort only, opt-in flag, confidence hard-capped below static sources, mock-only in CI. |

## Commit Decomposition

Each slice is a mergeable conventional commit with its test, fitting the
release-please flow:

**Phase 1**
1. `feat: add channel endpoint domain model and repository port`
2. `feat: add DuckDB channel endpoint repository`
3. `feat: add tree-sitter channel extractor with detector registry`
4. `feat: extract channel endpoints during indexing`
5. `feat: add channel link use case with wildcard and route matching`
6. `feat: add channels CLI command and MCP tool`

**Phase 2**
7. `feat: merge channel edges into file relationship graph`
8. `feat: weight channel edges in Leiden clustering` (+ `--channel-weight`)
9. `feat: label inter-cluster edges with connecting channels`
10. `feat: annotate execution features with channel triggers`
11. `feat: add cross-channel impact traversal behind --cross-channel`

**Phase 3**
12. `feat: resolve channel constants via import references`
13. `feat: parse config files and resolve config-keyed channels`
14. `feat: map HTTP hosts to repositories via namespace config and manifests`
15. `feat: derive gRPC channels from proto contracts`
16. `feat: add optional LLM resolution for unresolved channel endpoints`
