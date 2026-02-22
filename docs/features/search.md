# Search Features

## Overview

CodeSearch defaults to **hybrid search**: it combines semantic (vector) similarity with BM25-style keyword matching and fuses the two ranked lists using Reciprocal Rank Fusion (RRF). This gives you the recall of semantic search and the precision of keyword search in a single query.

## How It Works

```mermaid
flowchart TB
    A["Query string"] --> B[Embed Query]
    A --> D[Keyword leg\nBM25 LIKE matching]
    B --> C[Semantic leg\nVSS cosine search]
    C --> E[RRF Fusion]
    D --> E
    E --> F[min_score filter]
    F --> G[Reranking\noptional]
    G --> H[Search Results]

    B -.- B1[ONNX Runtime<br/>384-dim embeddings]
    C -.- C1[DuckDB VSS<br/>HNSW cosine distance]
    D -.- D1[LIKE on content<br/>+ symbol_name]
    E -.- E1[score = 1/(60+rank)<br/>summed across legs]
    G -.- G1[Cross-encoder reranker<br/>skip with --no-rerank]
```

**Default (Hybrid) Search Pipeline**:
1. **Query Embedding**: Input text is embedded with the same ONNX model used at indexing time (384 dimensions)
2. **Semantic leg**: DuckDB VSS HNSW index finds nearest vectors by cosine distance
3. **Keyword leg**: BM25-style `LIKE` matching on chunk content (1 pt) and symbol names (2 pts), normalised to [0, 1]
4. **RRF Fusion**: Both ranked lists are merged — each result scores `1 / (60 + rank)` from each leg it appears in; items found by both legs accumulate the highest fused scores (range ~0.016–0.033)
5. **Score filter**: `--min-score` applied once to the fused list
6. **Reranking**: Enabled by default. Semantic-only candidates below 0.1 cosine similarity are excluded before reranking; RRF results bypass this threshold because their scores are intentionally small
7. **Ranking**: Final order is fused RRF score (hybrid), cosine similarity (semantic-only), or cross-encoder score (reranked)

Pass `--no-text-search` to skip steps 3–4 and use pure semantic/vector search.

## Search Query Options

### Basic Search

```bash
# Hybrid search (default — semantic + keyword, fused via RRF)
codesearch search "parse configuration file"

# Semantic-only (vector similarity only, no keyword leg)
codesearch search "parse configuration file" --no-text-search
```

### Hybrid vs Semantic-only

| Mode | Command | When to use |
|------|---------|-------------|
| Hybrid (default) | `codesearch search "..."` | Best overall recall and precision; catches both semantic matches and exact keyword hits |
| Semantic-only | `codesearch search "..." --no-text-search` | Descriptive intent queries where keywords are unlikely to match; slightly faster |

> **Scoring**: Hybrid results use RRF scores (~0.016–0.033). Semantic-only results use cosine similarity (0.0–1.0). `--min-score` thresholds should be tuned accordingly.

### Result Limit

```bash
# Get top 20 results
codesearch search "error handling" --num 20
```

### Reranking for Better Relevance

Enable cross-encoder reranking to improve result quality:

```bash
# Basic reranking (fetches ~27 candidates via inverse-log scaling, returns top 10)
codesearch search "error handling"

# Reranking with custom result count
codesearch search "validation" --num 20

# No reranking
codesearch search "validation" --no-rerank
```

**How reranking works:**
- Fetches candidates from hybrid or vector search using an inverse-log formula: `num + ⌈num / ln(num)⌉` (defaults to 20 base candidates when `num ≤ 10`)
- For **semantic-only** results, filters out candidates with vector similarity score below 0.1 (too irrelevant to benefit from reranking)
- For **hybrid** results, the 0.1 threshold is bypassed — RRF scores are intentionally small (~0.016–0.033) and all fused results are passed to the reranker
- Rescores remaining candidates using a cross-encoder model (mxbai-rerank-xsmall-v1)
- Returns top `num` results by relevance score

**Trade-offs:**
- ✅ Better result relevance (especially for specific queries)
- ✅ No external dependencies or APIs
- ✅ Logarithmic candidate scaling keeps reranking fast even for large result counts

### Minimum Score Threshold

Filter out low-confidence matches:

```bash
# Only show results with score >= 0.5
codesearch search "database query" --min-score 0.5
```

### Language Filter

```bash
# Only search Rust code
codesearch search "async function" --language rust

# Multiple languages
codesearch search "http client" --language rust --language python
```

### Repository Filter

```bash
# Search specific repository
codesearch search "authentication" --repository abc123
```

## Output Formats

Use `-F` / `--format` to control the output format:

```bash
codesearch search "validate email" --format text    # default
codesearch search "validate email" --format json    # structured JSON
codesearch search "validate email" --format vimgrep # Neovim-compatible
```

### Text (default)

```text
Found 3 results:

1. src/auth/validator.rs:42-58 (score: 0.847)
   Symbol: validate_email (function)
   | pub fn validate_email(email: &str) -> bool {
   |     let re = Regex::new(r"^[^@]+@[^@]+\.[^@]+$").unwrap();
   |     re.is_match(email)

2. src/user/registration.rs:15-32 (score: 0.723)
   Symbol: check_email_format (function)
   | fn check_email_format(input: &str) -> Result<(), ValidationError> {
   |     if !input.contains('@') {
   |         return Err(ValidationError::InvalidEmail);

3. tests/validation_tests.rs:8-25 (score: 0.651)
   Symbol: test_email_validation (function)
   | #[test]
   |  fn test_email_validation() {
   |     assert!(validate_email("user@example.com"));
```

### JSON

Returns a JSON array of result objects, useful for scripts and editor integrations (e.g., the Telescope extension):

```json
[
  {
    "file_path": "src/auth/validator.rs",
    "start_line": 42,
    "end_line": 58,
    "score": 0.847,
    "language": "rust",
    "node_type": "function",
    "symbol_name": "validate_email",
    "content": "pub fn validate_email(email: &str) -> bool { ... }"
  }
]
```

### Vimgrep

Outputs `file:line:col:text` format, directly loadable into Neovim's quickfix list:

```text
src/auth/validator.rs:42:1:[0.847] validate_email - pub fn validate_email(email: &str) -> bool {
src/user/registration.rs:15:1:[0.723] check_email_format - fn check_email_format(input: &str) -> Result<(), ValidationError> {
```

```bash
# Open results in Neovim's quickfix list
codesearch search "validate email" --format vimgrep | nvim -q /dev/stdin
```

## Search Quality Tips

### Be Descriptive

```bash
# Good - describes the intent
codesearch search "function that connects to PostgreSQL database"

# Less effective - too generic
codesearch search "database"
```

### Include Context

```bash
# Good - includes what the code should do
codesearch search "middleware that handles authentication tokens"

# Good - describes the behavior
codesearch search "recursive function to traverse directory tree"
```

### Use Domain Language

```bash
# If your codebase uses specific terminology
codesearch search "CustomerOrder aggregate root validation"
```

## Similarity Scoring

Scores range from 0.0 to 1.0
