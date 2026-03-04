# Embedding Backends

CodeSearch supports two embedding backends, selected with `--embedding-target`.

## Backends at a Glance

| Backend | Flag | Default | Requires | Reranking |
|---------|------|---------|----------|-----------|
| ONNX (local) | `--embedding-target=onnx` | ✅ | Nothing — models downloaded on first run | Cross-encoder via ONNX |
| API (remote) | `--embedding-target=api` | — | Running LM Studio (or any OpenAI-compatible server) | LLM-based via `/v1/messages` |

---

## ONNX Backend (default)

Embeddings are generated locally using [ONNX Runtime](https://onnxruntime.ai) with a sentence-transformer model downloaded from HuggingFace Hub on first use. No external server is required.

```bash
# Default — no flag needed
codesearch index .
codesearch search "error handling middleware"

# Use a custom ONNX model (must match the dimension configured for the namespace)
codesearch index . --embedding-model sentence-transformers/all-mpnet-base-v2 --embedding-dimensions 768
```

### Default ONNX Model

| Model | Dimensions | Download size |
|-------|------------|---------------|
| `sentence-transformers/all-MiniLM-L6-v2` | 384 | ~90 MB |

### Reranking (ONNX)

When the ONNX backend is active, reranking uses `BAAI/bge-reranker-base` as a cross-encoder, downloaded automatically. Disable it with `--no-rerank`.

---

## API Backend

Embeddings are fetched from an OpenAI-compatible `/v1/embeddings` HTTP endpoint. The primary use case is [LM Studio](https://lmstudio.ai) running locally, but any compliant server works (Ollama, a remote OpenAI endpoint, etc.).

```bash
# Index with LM Studio running nomic-embed-text (768-dimensional)
codesearch index . \
  --embedding-target=api \
  --embedding-model=nomic-embed-text \
  --embedding-dimensions=768

# Search must use the same flags (validated against stored namespace config)
codesearch search "database connection pool" \
  --embedding-target=api \
  --embedding-model=nomic-embed-text \
  --embedding-dimensions=768
```

### Endpoint configuration

All API-target traffic shares the same environment variables used by query expansion:

| Variable | Default | Purpose |
|----------|---------|---------|
| `ANTHROPIC_BASE_URL` | `http://localhost:1234` | Base URL for embeddings (`/v1/embeddings`) and reranking (`/v1/messages`) |
| `ANTHROPIC_MODEL` | `mistralai/ministral-3-3b` | Chat model used for reranking |
| `ANTHROPIC_API_KEY` | `""` | API key — not required for local servers |

> LM Studio exposes both `/v1/embeddings` (OpenAI-compatible) and `/v1/messages` (Anthropic-compatible) on the same port, so one `ANTHROPIC_BASE_URL` covers everything.

### Reranking (API)

When the API backend is active, reranking uses the same LM Studio server via `/v1/messages`. The model is prompted once with all candidates and asked to output a JSON array of relevance scores. This is LLM-based reranking — different from the ONNX cross-encoder but effective for general queries.

On any error (server unreachable, unparseable response, wrong array length) the reranker falls back silently to the original retrieval scores.

### LM Studio setup

1. Download and start [LM Studio](https://lmstudio.ai).
2. Load an embedding model (e.g. `nomic-ai/nomic-embed-text-v1.5`) in the **Local Server** tab.
3. Note the output dimensions from the model card (e.g. 768 for nomic-embed-text).
4. Optionally load a second chat model for reranking/query expansion, or use a single model for all tasks if it supports both.
5. Start the server on the default port (`1234`).

---

## Namespace Config and Dimension Enforcement

The embedding configuration for a namespace is written to the `namespace_config` table in DuckDB the **first time** that namespace is indexed. Every subsequent open — whether for indexing or searching — validates the provided config against the stored one.

### What is stored

```
namespace_config
├── namespace        (e.g. "search")
├── embedding_target (e.g. "onnx" or "api")
├── embedding_model  (e.g. "sentence-transformers/all-MiniLM-L6-v2")
└── dimensions       (e.g. 384)
```

The `embeddings` table schema is created with `FLOAT[{dimensions}]`, so the column type is fixed at namespace creation time.

### Validation rules

| Condition | Result |
|-----------|--------|
| Stored dimensions ≠ requested | **Hard error** — schema incompatible |
| Stored model ≠ requested model | **Hard error** — different embedding space |

Both errors include an actionable message:

```
Namespace 'search' was indexed with 384-dimensional embeddings
(model 'sentence-transformers/all-MiniLM-L6-v2', target 'onnx')
but you are now using 768-dimensional embeddings
(model 'nomic-embed-text', target 'api').
Re-index with `codesearch index --force` using the original model,
or create a new namespace with `--namespace <name>`.
```

### Switching models or dimensions

**Option A — Re-index the same namespace:**
```bash
codesearch index . --force --embedding-model nomic-embed-text --embedding-dimensions 768 --embedding-target api
```
`--force` rebuilds from scratch, replacing the stored config with the new one.

**Option B — Use a separate namespace:**
```bash
codesearch index . \
  --namespace my-repo-768 \
  --embedding-target api \
  --embedding-model nomic-embed-text \
  --embedding-dimensions 768

codesearch search "query" \
  --namespace my-repo-768 \
  --embedding-target api \
  --embedding-model nomic-embed-text \
  --embedding-dimensions 768
```

---

## CLI Reference

All embedding flags are global — they apply to `index`, `search`, `mcp`, and any other subcommand that opens the vector store.

```
--embedding-target <onnx|api>      Embedding backend (default: onnx)
--embedding-model <name>           HuggingFace ID (onnx) or API model name
--embedding-dimensions <n>         Output dimensions (default: 384)
```

These flags should be passed consistently across `index` and `search` — the namespace config validator will catch mismatches, but it's easiest to set them in a shell alias or wrapper script.

### Example aliases

```bash
# ~/.bashrc or ~/.zshrc

# ONNX (default) — no flags needed
alias cs='codesearch'

# LM Studio with nomic-embed-text (768-dim)
alias cs-lm='codesearch \
  --embedding-target=api \
  --embedding-model=nomic-embed-text \
  --embedding-dimensions=768'
```

---

## Choosing a Backend

| Factor | ONNX | API |
|--------|------|-----|
| Internet required for indexing | First run only (model download) | Never |
| Embedding quality | Good — sentence-transformers | Depends on loaded model |
| Indexing speed | Fast (local inference) | Depends on server |
| Memory usage | ~200 MB (model in RAM) | Minimal |
| Model flexibility | Any HuggingFace ONNX model | Any model LM Studio supports |
| Reranking | Cross-encoder (BAAI/bge-reranker-base) | LLM-based (same chat model) |
