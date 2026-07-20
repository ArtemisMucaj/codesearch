# Serve & Management API

`codesearch serve` runs two servers in one process:

1. The **MCP server** over HTTP (the same Model Context Protocol server as
   `codesearch mcp --http`, for AI agents), and
2. A **REST/JSON + SSE management API** — a plain HTTP surface over codesearch's
   operations, meant for scripts, dashboards, and native front-ends that want to
   drive a running index without speaking MCP.

It also starts the **memory-dream scheduler** in the background (see
[Long-Term Memory — Dreaming](./memory.md#dreaming)). Both servers shut down
gracefully on ctrl-c.

```bash
# MCP on 8677, management API on 8676 (defaults), bound to 127.0.0.1
codesearch serve

# Custom ports
codesearch serve --mcp-port 3000 --mgmt-port 3001

# Bind both on all interfaces (0.0.0.0) instead of localhost
codesearch serve --public
```

| Flag | Default | Description |
|---|---|---|
| `--mcp-port` | `8677` | Port for the MCP HTTP server (endpoint at `/mcp`) |
| `--mgmt-port` | `8676` | Port for the REST/JSON + SSE management API |
| `--public` | off | Bind `0.0.0.0` instead of `127.0.0.1` |

## MCP vs. management API

| | MCP server | Management API |
|---|---|---|
| Protocol | Model Context Protocol (JSON-RPC) | REST/JSON + Server-Sent Events |
| Audience | AI agents / MCP clients | Scripts, dashboards, native apps |
| Endpoint | `/mcp` (on `--mcp-port`) | `/api/...` (on `--mgmt-port`) |
| Tools/routes | 20 tools ([Editor Integrations](./editor-integrations.md#exposed-mcp-tools)) | ~25 REST routes + 2 streaming routes |

To run **only** the MCP server (no management API), use `codesearch mcp` (stdio)
or `codesearch mcp --http <port>`.

## The OpenAPI contract

The management API's full contract — every path, query parameter, request body,
response shape, and error — is the checked-in spec at
[`docs/management-api.openapi.json`](../management-api.openapi.json). The running
server serves that same document verbatim at **`GET /api/openapi.json`**, so a
client can always fetch the contract that matches the binary it's talking to.

Treat the OpenAPI file as the source of truth; this page is the orientation.

## REST endpoints

All request/response endpoints live under `/api/...` (excluding
`/api/stream/...`). Errors are returned as `{ "error": "<message>" }` with an
appropriate status: `400` for malformed input (unknown protocol / memory kind,
bad body), `404` when a named repository, symbol, or memory item is not found,
and `500` for any other use-case failure.

### Health & discovery

| Method & path | Purpose |
|---|---|
| `GET /health` | Liveness probe → `{"status":"ok","version":"…"}` |
| `GET /api` | API index (also `GET /`) |
| `GET /api/openapi.json` | The OpenAPI spec for this server |

### Repositories & stats

| Method & path | Purpose |
|---|---|
| `GET /api/repositories` | List indexed repositories |
| `GET /api/repositories/{id}` | One repository |
| `DELETE /api/repositories/{id}` | Delete a repository from the index |
| `GET /api/stats` | Index statistics |

### Search & call graph

| Method & path | Purpose |
|---|---|
| `POST /api/search` | Hybrid/semantic search (query + filters in the body) |
| `POST /api/impact` | Blast-radius analysis for a symbol |
| `GET /api/context/{symbol}` | 360° caller/callee context |
| `GET /api/uses` | Cross-repository file dependencies (`from`, `to`) |

### Architecture analysis

| Method & path | Purpose |
|---|---|
| `GET /api/features` | Entry-point execution features by criticality |
| `GET /api/clusters` | File-level Leiden clusters (architectural modules); `?global=true` runs one namespace-wide detection across every repository (members become `repo:path`) |
| `GET /api/symbol-clusters` | Symbol-level Leiden communities |
| `GET /api/graph` | Render-ready community graph with edges (`level=file\|symbol`, `aggregate=`, `global=` for the namespace-wide file graph) |
| `GET /api/couplings` | Coupling elements (`repository`, `level=file\|symbol`) |
| `GET /api/channels` | Cross-service channel links |

### Memory

| Method & path | Purpose |
|---|---|
| `GET /api/memory` | List memory items (`kind` filter) |
| `GET /api/memory/{id}` | One memory item |
| `GET /api/memory/search` | Hybrid memory recall |
| `GET /api/memory/tree` | Browse the `memory://` virtual filesystem |
| `GET /api/memory/sessions` | Imported sessions |
| `GET /api/memory/stats` | Memory-store statistics |
| `GET /api/memory/dream` | Dream-scheduler status + last run |
| `POST /api/memory/dream` | Trigger a dream cycle in the background |

### LLM backend management

The management API can configure LLM backends against a **running** server —
useful for a native app that lets a user pick a provider/model on the fly:

| Method & path | Purpose |
|---|---|
| `GET /api/llm/endpoints` | List OpenAI-compatible endpoints (API keys masked) |
| `PUT /api/llm/endpoints/{name}` | Add/update an endpoint (write-only `api_key`) |
| `POST /api/llm/active` | Set the active endpoint |
| `GET /api/llm/models` | List a backend's models (`?target=openai\|copilot&endpoint=<name>`) |

Model discovery is uniform for OpenAI (`/v1/models`) and Copilot (`/models`);
the Anthropic Messages API has no portable discovery endpoint and is
intentionally excluded. See [AGENTS.md — LLM backends](../../AGENTS.md#llm-backends).

## Streaming (SSE) endpoints

Paths under `/api/stream/` respond with `text/event-stream`. Each frame is a
named SSE event (`event:`) carrying a JSON `data:` payload. A terminal `done` or
`error` event is always the last frame — clients treat either as end-of-stream.
If the client disconnects, the server drops the in-flight work.

| Method & path | Streams |
|---|---|
| `POST /api/stream/index` | Live indexing progress |
| `GET` / `POST /api/stream/explain/{symbol}` | Token-by-token LLM explanation |

Event names:

| Event | Emitted by | Payload |
|---|---|---|
| `progress` | `index` | Per-stage indexing progress |
| `token` | `explain` | One chunk of the streamed LLM explanation |
| `done` | both | Terminal success. For `explain`, a `status` field distinguishes a normal result from `"ambiguous"` (the symbol matched more than one candidate, listed under `candidates`). |
| `error` | both | Terminal failure (`{"message":"…"}`) |

The event names and payloads are mirrored in the `Sse*` component schemas of the
OpenAPI spec (OpenAPI can't express SSE frame shapes natively).

## Example

```bash
# Start the servers
codesearch serve &

# Liveness
curl -s localhost:8676/health

# Search
curl -s localhost:8676/api/search \
  -H 'content-type: application/json' \
  -d '{"query":"retry logic for network timeouts","limit":5}'

# Stream an indexing run
curl -N localhost:8676/api/stream/index \
  -H 'content-type: application/json' \
  -d '{"path":"/path/to/repo"}'

# Fetch the live contract
curl -s localhost:8676/api/openapi.json | jq '.paths | keys'
```

> **Binding & exposure:** `--public` binds `0.0.0.0`. The management API has no
> built-in authentication, so only expose it publicly behind your own
> authenticated proxy or on a trusted network.
