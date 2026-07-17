# Embedding Backends

CodeSearch turns code into vectors with one of two embedding backends. Which
backend, which model, and how many dimensions are **properties of a namespace**,
fixed once and inherited by every `index` and `search` that runs against it —
you do not repeat embedding flags on each command.

## Backends at a glance

| Backend | `--embedding-target` | Default | Requires | Reranking |
|---------|----------------------|---------|----------|-----------|
| **ONNX** (local) | `onnx` | ✅ | Nothing — model downloaded on first run | Cross-encoder via ONNX |
| **API** (remote) | `api` | — | An OpenAI-compatible `/v1/embeddings` server (LM Studio, vLLM, Ollama, hosted OpenAI, …) | LLM-based, or ONNX |

The embedding target is chosen when a namespace is created (see below). The
**reranking** backend is independent and set per-run with the global
`--reranking-target` flag (`onnx`, `api/anthropic`, or `api/openai`).

## How a namespace gets its embedding config

A namespace's embedding configuration is decided **once**, in one of two ways:

1. **Explicitly, with `codesearch create`** — before indexing:

   ```bash
   # A namespace backed by a remote embedding model (768-dim)
   codesearch create my-project \
     --embedding-target api \
     --embedding-model nomic-embed-text \
     --embedding-dimensions 768

   # A keyword + call-graph-only namespace — skips the embed stage entirely
   # (no model download, no inference); search uses the keyword + call-graph legs
   codesearch create fast-index --no-embeddings
   ```

2. **Implicitly, on first `index`** — indexing a namespace that was never
   created configures it with the **defaults**: ONNX,
   `sentence-transformers/all-MiniLM-L6-v2`, 384 dimensions.

   ```bash
   codesearch index .        # first index → namespace "search" gets the defaults
   ```

`create` flags (only valid on `create`):

| Flag | Default | Description |
|---|---|---|
| `--embedding-target` | `onnx` | `onnx` (bundled, offline) or `api` (OpenAI-compatible endpoint) |
| `--embedding-model` | `all-MiniLM-L6-v2` (onnx) | HuggingFace ID (onnx) or model name (api); **required** for `api` |
| `--embedding-dimensions` | `384` | Output dimensions of the model |
| `--no-embeddings` | off | Create a keyword + call-graph-only namespace (no embed stage) |

After creation, `index` and `search` read the stored config and need no
embedding flags:

```bash
codesearch index . --namespace my-project
codesearch search "database connection pool" --namespace my-project
```

Run from inside the repo and even `--namespace` is auto-resolved — see
[Automatic namespace resolution](./indexing.md#automatic-namespace-resolution).

---

## ONNX backend (default)

Embeddings are generated locally with [ONNX Runtime](https://onnxruntime.ai)
using a sentence-transformer model downloaded from HuggingFace Hub on first use.
No external server, no API key.

| Default model | Dimensions | Download size |
|---|---|---|
| `sentence-transformers/all-MiniLM-L6-v2` | 384 | ~90 MB |

```bash
# Default — nothing to configure
codesearch index .
codesearch search "error handling middleware"

# A different ONNX model — set it at namespace-creation time so the stored
# dimensions match the model
codesearch create big-model \
  --embedding-model sentence-transformers/all-mpnet-base-v2 \
  --embedding-dimensions 768
codesearch index . --namespace big-model
```

**Reranking (ONNX):** when the reranking target is `onnx` (the default),
reranking uses `BAAI/bge-reranker-base` as a cross-encoder, downloaded
automatically. Disable reranking entirely with `--no-rerank`.

---

## API backend

Embeddings are fetched from an OpenAI-compatible `/v1/embeddings` HTTP endpoint.
The common case is [LM Studio](https://lmstudio.ai) or vLLM running locally, but
any compliant server works.

```bash
# Create the namespace with the API backend and the model's dimensions
codesearch create local-api \
  --embedding-target api \
  --embedding-model nomic-embed-text \
  --embedding-dimensions 768

# Then just index and search — no embedding flags needed
codesearch index .  --namespace local-api
codesearch search "database connection pool" --namespace local-api
```

The OpenAI-compatible endpoint used for API embeddings is configured the same
way as the OpenAI LLM backend — via `codesearch openai …` or the `OPENAI_*`
environment variables (`OPENAI_BASE_URL`, `OPENAI_MODEL`, `OPENAI_API_KEY`). See
[AGENTS.md — LLM backends](../../AGENTS.md#openai-compatible-endpoints).

### LM Studio setup

1. Download and start [LM Studio](https://lmstudio.ai).
2. Load an embedding model (e.g. `nomic-ai/nomic-embed-text-v1.5`) in the
   **Local Server** tab, and note its output dimensions from the model card.
3. Start the server on the default port (`1234`).
4. `codesearch create <ns> --embedding-target api --embedding-model <name>
   --embedding-dimensions <n>`, then index into `<ns>`.

---

## Reranking backends

Reranking (rescoring the retrieved candidates) is separate from embedding and
selected per-run with the global `--reranking-target`:

| `--reranking-target` | Method | Model |
|---|---|---|
| `onnx` (default) | Cross-encoder (ONNX) | `BAAI/bge-reranker-base`, downloaded on first use |
| `api/anthropic` | LLM-based (one prompt → JSON scores) | Anthropic-compatible `/v1/messages` (`ANTHROPIC_*`) |
| `api/openai` | LLM-based (one prompt → JSON scores) | OpenAI-compatible `/v1/chat/completions` (`OPENAI_*`) |

LLM-based reranking prompts the model once with all candidates and asks for a
JSON array of relevance scores. On any error (server unreachable, unparseable
response, wrong length) it falls back silently to the original retrieval scores.
`--no-rerank` skips reranking altogether.

---

## Dimension & model enforcement

The chosen backend, model, and dimensions are written to the `namespace_config`
DuckDB table when the namespace is created (or first indexed). The `embeddings`
column is typed `FLOAT[{dimensions}]`, so the width is fixed for the life of the
namespace. Every later open validates against the stored config:

| Condition | Result |
|---|---|
| Stored dimensions ≠ current model's dimensions | **Hard error** — schema incompatible |
| Stored model ≠ the model producing the vectors | **Hard error** — different embedding space |

To change the model or dimensions, either re-index with `--force` (which
rebuilds the namespace) or create a new namespace with the new configuration.

---

## Choosing a backend

| Factor | ONNX | API |
|--------|------|-----|
| Internet for indexing | First run only (model download) | Never (local server) |
| Embedding quality | Good — sentence-transformers | Depends on the loaded model |
| Indexing speed | Fast (local inference) | Depends on the server |
| Memory usage | ~200 MB (model in RAM) | Minimal (in codesearch) |
| Model flexibility | Any HuggingFace ONNX model | Any model the server supports |
| Setup | None | Run and configure the endpoint |

**Rule of thumb:** stick with the ONNX default unless you specifically want a
particular embedding model served from a local or hosted OpenAI-compatible
endpoint.
