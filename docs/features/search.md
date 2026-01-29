# Search Features

## Overview

CodeSearch uses semantic similarity to find relevant code. Unlike keyword search, it understands the meaning of your query and finds conceptually similar code.

## How It Works

```text
Query: "function to validate email addresses"
                    │
                    ▼
            ┌───────────────┐
            │  Embed Query  │ ─── Same model as indexing
            └───────────────┘
                    │
                    ▼
            ┌───────────────┐
            │ Vector Search │ ─── Find nearest neighbors
            └───────────────┘
                    │
                    ▼
            ┌───────────────┐
            │ Apply Filters │ ─── Language, score, etc.
            └───────────────┘
                    │
                    ▼
            ┌───────────────┐
            │ Fetch Details │ ─── Get full chunk from SQLite
            └───────────────┘
                    │
                    ▼
              Search Results
```

## Search Query Options

### Basic Search

```bash
codesearch search "parse configuration file"
```

### Result Limit

```bash
# Get top 20 results
codesearch search "error handling" --limit 20
```

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

Scores range from 0.0 to 1.0:

| Score Range | Meaning                          |
|-------------|----------------------------------|
| 0.8 - 1.0   | Highly relevant, exact matches   |
| 0.6 - 0.8   | Good matches, related code       |
| 0.4 - 0.6   | Somewhat relevant                |
| < 0.4       | Weak matches                     |

## Programmatic Search

From Rust code:

```rust
use codesearch_application::SearchCodeUseCase;
use codesearch_domain::SearchQuery;

let query = SearchQuery::new("validate user input")
    .with_limit(10)
    .with_min_score(0.5)
    .with_languages(vec!["rust".to_string()]);

let results = search_use_case.execute(query).await?;

for result in results {
    println!("{}: {}", result.chunk.location(), result.score);
}
```

## Future Improvements

- [ ] Hybrid search (semantic + keyword)
- [ ] Code-specific query preprocessing
- [ ] Result re-ranking with cross-encoder
- [ ] Search history and bookmarks
- [ ] Natural language query expansion
