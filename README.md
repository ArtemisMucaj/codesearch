# codesearch

**Semantic code search powered by embeddings** — index a repository once, then
find code by *what it does*, understand how symbols connect, and see how the
whole thing is structured. A single Rust binary. No external services required.

```bash
codesearch index .
codesearch search "where do we validate the auth token before issuing a session"
```

---

## What it does

codesearch is three tools in one binary, all built on a single index:

- **Search** — hybrid retrieval that fuses a semantic (vector) leg with a
  keyword (BM25-style) leg via Reciprocal Rank Fusion, then reranks with a
  cross-encoder. Finds code by meaning *and* by exact token, in one query.
- **Understand** — a call graph built during indexing powers blast-radius
  (`impact`), 360° caller/callee context (`context`), and LLM-written
  call-flow explanations (`explain`).
- **Map** — file- and symbol-level community detection (Leiden), coupling
  hotspots, entry-point execution features, cross-service channels, and a
  one-page repository dossier (`overview`) — plus interactive graph rendering.

It also ships an **MCP server** (so AI agents can call it), a **REST + SSE
management API**, an **interactive TUI**, editor integrations (Neovim,
Zed), and a **long-term memory** subsystem that distills finished assistant
sessions into searchable knowledge.

**Languages:** Rust, Python, JavaScript, TypeScript, Go, HCL/Terraform, PHP,
C++. JavaScript/TypeScript and PHP get a precise call graph via SCIP
(`scip-typescript` / `scip-php`); every language gets tree-sitter chunk
extraction.

---

## Install

### From a release (recommended)

```bash
# Downloads the latest release binary for your OS/arch into ~/.local/bin
INSTALL_DIR="$HOME/.local/bin" sh .claude/skills/codesearch-cli/install.sh
codesearch --version
```

Make sure `~/.local/bin` is on your `PATH`.

### From source

```bash
git clone https://github.com/ArtemisMucaj/codesearch
cd codesearch
cargo build --release        # binary at ./target/release/codesearch
cargo install --path .       # or install to ~/.cargo/bin
```

No system dependencies — DuckDB and ONNX Runtime are bundled through Cargo.
(For sandboxed/offline builds where the ONNX Runtime download is blocked, see
[AGENTS.md](AGENTS.md#building-in-a-sandboxed--offline-environment).)

---

## Quick start

```bash
# 1. Index a repository (incremental on re-run — only changed files re-parse)
codesearch index /path/to/repo

# 2. Search it
codesearch search "retry logic for network timeouts"

# 3. Understand a symbol
codesearch impact authenticate       # what breaks if this changes?
codesearch context authenticate      # who calls it / what it calls
codesearch explain authenticate      # LLM-written call-flow summary

# 4. Map the codebase
codesearch overview                  # one-page dossier for the current repo

# 5. Serve it to your editor / AI agent
codesearch mcp                       # MCP server over stdio
```

Run commands from *inside* a repository and codesearch resolves the right
namespace and embedding configuration automatically — you rarely need any
global flags. See [Namespaces & automatic resolution](#namespaces--automatic-resolution).

---

## Commands

| Command | What it does |
|---|---|
| `index <path>` | Parse, embed, and store a repository for search |
| `search <query>` | Hybrid semantic + keyword search |
| `list` / `stats` | List indexed repositories / show index statistics |
| `delete <id-or-path>` | Remove a repository from the index |
| `create [name]` | Create a namespace with a fixed embedding configuration |
| `impact <symbol>` | Blast radius of changing a symbol (BFS over the call graph) |
| `context <symbol>` | 360° caller/callee call-chain tree for a symbol |
| `explain <symbol>` | LLM explanation of a symbol's call flow & business purpose |
| `features <sub>` | Entry-point execution flows ranked by criticality |
| `clusters <sub>` | Architectural modules — Leiden over the file graph |
| `symbol-clusters <sub>` | Behavioural communities — Leiden over the call graph |
| `couplings` | Files/edges whose removal would split a community in two |
| `channels` | Cross-service links (Kafka, HTTP, MQTT, AMQP, gRPC) |
| `uses <from> <to>` | Files in one repo that reference symbols in another |
| `overview` | One-page Markdown dossier combining every analysis |
| `visualize` | Render communities as interactive HTML, SVG, or Obsidian canvas |
| `memory <sub>` | Long-term memory from finished assistant sessions |
| `tui` | Interactive terminal UI (search + impact + context) |
| `mcp` | Start the MCP server (stdio or HTTP) |
| `serve` | Run the MCP server **and** the REST/SSE management API together |
| `copilot <sub>` / `openai <sub>` | Configure LLM backends |

Every command has `--help`. Full reference lives in [`docs/`](docs/README.md).

### Global flags

These apply to any subcommand that opens the index:

| Flag | Default | Description |
|---|---|---|
| `-d, --data-dir <dir>` | `~/.codesearch` | Directory for the DuckDB database and `config.json` |
| `--namespace <ns>` | `search` | DuckDB schema namespace (usually auto-resolved) |
| `--memory-storage` | off | Ephemeral in-memory storage (no persistence) |
| `--mock-embeddings` | off | Deterministic mock embeddings (testing) |
| `--no-rerank` | off | Skip the cross-encoder reranking stage |
| `--expand-query` | off | Expand the query into LLM-generated variants, fuse via RRF |
| `--reranking-target <t>` | `onnx` | `onnx`, `api/anthropic`, or `api/openai` |
| `--llm-target <t>` | `open-ai` | LLM backend: `open-ai`, `anthropic`, or `copilot` |
| `-v, --verbose` | off | Debug-level logging |

---

## Search

By default `search` runs **hybrid** retrieval: a semantic vector leg and a
keyword leg, fused with Reciprocal Rank Fusion (each hit scores `1/(60+rank)`
per leg it appears in), then reranked by a cross-encoder.

```bash
codesearch search "parse and validate a configuration file"   # hybrid (default)
codesearch search "..." --no-text-search                      # semantic-only
codesearch search "async task queue" --num 25                 # more results
codesearch search "struct definition" --language rust         # filter by language
codesearch search "config loading" --repository my-project    # filter by repo
codesearch search "..." --format json                         # JSON for tooling
codesearch search "..." --format vimgrep | nvim -q /dev/stdin # Neovim quickfix
```

| Flag | Default | Description |
|---|---|---|
| `--num` | `10` | Number of results |
| `-m, --min-score` | (none) | Minimum relevance score (see scoring note below) |
| `-L, --language` | (none) | Filter by language (repeatable) |
| `-r, --repository` | (none) | Filter by repository (repeatable) |
| `-F, --format` | `text` | `text`, `json`, or `vimgrep` |
| `--no-text-search` | off | Disable the keyword leg (pure semantic search) |

> **Scoring:** hybrid RRF scores land in ~0.016–0.033; semantic-only cosine
> scores are 0.0–1.0. Tune `--min-score` to whichever mode you're in.

See [docs/features/search.md](docs/features/search.md) for the full pipeline
and [docs/features/embedding-backends.md](docs/features/embedding-backends.md)
for local (ONNX) vs API embedding backends.

---

## Understand: the call graph

`index` builds a call graph — caller→callee edges with reference kind and
location. Three commands query it.

```bash
codesearch impact authenticate         # everything transitively affected by a change
codesearch context authenticate        # callers (as trees) + callees hanging off the symbol
codesearch explain authenticate        # LLM-written purpose, data/control flow, business feature
```

All three accept `-r/--repository`, `-F/--format` (`text`/`json`/`vimgrep`, except
`explain`), and resolve the symbol by **substring** by default — pass `--regex`
to supply your own anchored POSIX pattern:

```bash
codesearch impact "^MyNs/.*Service#get$" --regex
```

`explain` needs an LLM backend (defaults to a local OpenAI-compatible endpoint;
select another with `--llm`). Full reference:
[docs/features/call-graph.md](docs/features/call-graph.md).

---

## Map: architecture & dependency analysis

```bash
codesearch overview                          # one-page dossier for the current repo
codesearch features list my-repo             # entry-point flows by criticality
codesearch clusters list my-repo             # architectural modules (file-level Leiden)
codesearch symbol-clusters list my-repo      # behavioural communities (symbol-level Leiden)
codesearch couplings -r my-repo              # what glues a community together
codesearch channels                          # cross-service Kafka/HTTP/MQTT links
codesearch uses web core                     # files in `web` that use `core`
codesearch visualize my-repo -o graph.html   # interactive community graph
```

`overview` combines all of the above into a single Markdown report (index
stats, modules, communities, coupling hotspots, critical features, channels,
and an optional LLM executive summary), degrading gracefully when a section's
data is missing. Detected clusters and features are cached in DuckDB and
invalidated automatically on re-index.

Full reference:
[docs/features/architecture-analysis.md](docs/features/architecture-analysis.md).

---

## Namespaces & automatic resolution

A namespace is a DuckDB schema with a fixed embedding configuration (backend,
model, dimensions), decided once at creation and inherited by every later
`index` and `search` against it. Indexing into a namespace that was never
explicitly created configures it with the defaults (ONNX,
`all-MiniLM-L6-v2`, 384 dimensions).

```bash
# Create a namespace with a specific embedding setup (optional)
codesearch create my-project --embedding-target api \
  --embedding-model nomic-embed-text --embedding-dimensions 768

# Or a lightweight keyword + call-graph-only namespace (no embed stage)
codesearch create fast-index --no-embeddings
```

At index time codesearch records the repository's **normalized git remote**
(e.g. `github.com/owner/repo`). Any later command run from inside that repo
does a cheap read-only lookup and adopts the correct namespace and embedding
config automatically — no flags needed, and it survives re-clones and moves
because the key is the remote, not the path. An explicit `--namespace` always
wins. See
[docs/features/indexing.md](docs/features/indexing.md#automatic-namespace-resolution).

---

## Integrations

### MCP server (AI agents)

```bash
codesearch mcp                       # stdio (Claude Desktop, Cursor, Zed, …)
codesearch mcp --http 8080           # HTTP; endpoint at /mcp
codesearch mcp --http 8080 --public  # bind 0.0.0.0
```

Exposes 18 tools: `search_code`, `analyze_impact`, `get_symbol_context`,
`query_graph`, `list_repositories`, `list_features`, `get_feature`,
`get_impacted_features`, `file_uses`, `list_clusters`, `get_file_cluster`,
`list_symbol_clusters`, `get_symbol_cluster`, `couplings`, `channels`,
`search_memory`, `list_memories`, and `read_memory`. `query_graph` supports
eight intention-named patterns (`callers_of`, `callees_of`, `imports_of`,
`importers_of`, `inheritors_of`, `children_of`, `tests_for`, `file_summary`).

### `serve` — MCP + management API

```bash
codesearch serve                            # MCP on :8677, management API on :8676
codesearch serve --mcp-port 3000 --mgmt-port 3001
codesearch serve --public
```

`serve` runs the MCP HTTP server and a **REST/JSON + SSE management API** side
by side, and schedules memory dreaming in the background. The management API
covers search, call-graph, clusters, couplings, channels, memory, LLM backend
management, and streaming (`/api/stream/index`, `/api/stream/explain/{symbol}`).
The full contract is the checked-in OpenAPI spec at
[`docs/management-api.openapi.json`](docs/management-api.openapi.json) (served
verbatim at `GET /api/openapi.json`). Overview:
[docs/features/serve-and-management-api.md](docs/features/serve-and-management-api.md).

### Editor integrations

- **Neovim / Telescope** — a fuzzy picker over semantic results (`ide/nvim/`).
- **Zed** — MCP context server, command-palette tasks, and keybindings
  (`ide/zed/`).

See [docs/features/editor-integrations.md](docs/features/editor-integrations.md).

### Agent skills

Two [agent skills](https://skills.md) ship in `.claude/skills/`, teaching an AI
assistant how to drive codesearch as a runbook (recall memory → map the
architecture → search by intent → trace call graph):

| Skill | Use it when | Surface |
|---|---|---|
| `codesearch-cli` | The assistant runs shell commands | The `codesearch` CLI |
| `codesearch-mcp` | The codesearch MCP server is connected | The MCP tools |

Install with the [`skills`](https://www.npmjs.com/package/skills) CLI. Because
the repo ships two skills, pick which with `--skill`:

```bash
# Install just one
npx skills add ArtemisMucaj/codesearch --skill codesearch-cli
npx skills add ArtemisMucaj/codesearch --skill codesearch-mcp

# Or both, globally (user-level), without prompts
npx skills add ArtemisMucaj/codesearch --skill codesearch-cli codesearch-mcp -g -y

# Preview what's available first
npx skills add ArtemisMucaj/codesearch --list
```

Omit `-g` to install into the current project (`.claude/skills/`). The two are
independent — `codesearch-cli` bundles the binary `install.sh`; `codesearch-mcp`
assumes the MCP server is already connected.

### Interactive TUI

```bash
codesearch tui                       # search mode
codesearch tui --mode impact         # impact mode
codesearch tui --query "auth flow"   # pre-populate and dispatch
```

---

## Long-term memory

Import finished assistant sessions (Claude Code transcripts or generic JSONL
chat logs) and distill them into durable, searchable memories — preferences,
experiences, skills, and facts — in a separate `memory.duckdb`.

```bash
codesearch memory import ~/.claude/projects/<project>/<session>.jsonl
codesearch memory search "how do we handle lock conflicts"
codesearch memory add https://example.com/guide --name guide   # add a file/URL
codesearch memory tree                                          # browse the memory:// VFS
codesearch memory dream                                         # consolidate the store
```

`serve` schedules importing and consolidation automatically. Full design:
[docs/features/memory.md](docs/features/memory.md).

---

## LLM backends

LLM features (`explain`, community naming, query expansion, memory extraction,
dreaming) run through one of three interchangeable backends, selected with the
global `--llm-target`:

| Target | Backend | Configure with |
|---|---|---|
| `open-ai` (default) | OpenAI-compatible `/v1/chat/completions` (LM Studio, vLLM, hosted OpenAI) | `codesearch openai …` or `OPENAI_*` env vars |
| `anthropic` | Anthropic-compatible `/v1/messages` | `ANTHROPIC_*` env vars |
| `copilot` | A GitHub Copilot subscription | `codesearch copilot login` |

```bash
codesearch openai add lmstudio --base-url http://localhost:1234 --set-active
codesearch copilot login
codesearch explain some_function --llm copilot
```

See [AGENTS.md — LLM backends](AGENTS.md#llm-backends) for the full details,
including runtime backend management through the `serve` API.

---

## Development

```bash
cargo build --release
cargo test                    # in-memory storage + mock embeddings; no network
cargo fmt && cargo clippy
```

Architecture (Domain-Driven Design, Ports & Adapters), conventions, commit
style, and contribution workflows are documented in **[AGENTS.md](AGENTS.md)**
(the canonical agent & contributor guide; `CLAUDE.md` is a symlink to it).

## Documentation

- **[docs/](docs/README.md)** — the full documentation index
- **[AGENTS.md](AGENTS.md)** — architecture & contributor guide

## License

MIT
