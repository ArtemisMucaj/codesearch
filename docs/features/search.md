# Search Features

## Overview

CodeSearch uses semantic similarity to find relevant code. Unlike keyword search, it understands the meaning of your query and finds conceptually similar code.

## How It Works

```mermaid
flowchart TB
    A["Query: 'function to validate email addresses'"] --> B[Embed Query]
    B --> C[Vector Search with VSS]
    C --> D[Apply Filters]
    D --> E[Fetch Details]
    E --> F[Search Results]

    B -.- B1[ONNX Runtime<br/>Same model as indexing]
    C -.- C1[DuckDB VSS<br/>HNSW index<br/>Cosine distance]
    D -.- D1[Language, score,<br/>repository filters]
    E -.- E1[Get full chunk<br/>from DuckDB]
```

**Search Pipeline**:
1. **Query Embedding**: Input text is embedded using the same model as indexing (384 dimensions)
2. **VSS Search**: DuckDB's Vector Similarity Search uses HNSW index to find similar vectors (fast approximate nearest neighbors)
3. **Filtering**: Results filtered by language, node type, repository, and minimum score threshold
4. **Reranking**: Enabled by defaylt, skip using `--no-rerank`
5. **Details Fetch**: Full code chunks reconstructed from DuckDB
6. **Ranking**: Results ranked by cosine distance (0.0 = opposite, 1.0 = identical) or reranking score if enabled

## Search Query Options

### Basic Search

```bash
codesearch search "parse configuration file"
```

### Result Limit

```bash
# Get top 20 results
codesearch search "error handling" --num 20
```

### Reranking for Better Relevance

Enable cross-encoder reranking to improve result quality:

```bash
# Basic reranking (fetches 100 candidates, returns top 10)
codesearch search "error handling"

# Reranking with custom result count (fetches 200, returns top 20)
codesearch search "validation" --num 20

# No reranking
codesearch search "validation" --no-rerank
```

**How reranking works:**
- Fetches candidates from vector search (minimum 100, or `num × 10` if `num > 10`)
- Rescores them using a cross-encoder model (mxbai-rerank-xsmall-v1)
- Returns top `num` results by relevance score

**Trade-offs:**
- ✅ Better result relevance (especially for specific queries)
- ✅ No external dependencies or APIs
- ⚠️ ~2-5x slower than vector-only search

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

## Search Result Format

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
