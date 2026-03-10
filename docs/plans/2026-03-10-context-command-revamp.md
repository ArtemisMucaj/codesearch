# Context Command Revamp — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Revamp the `context` command to show a full end-to-end call chain tree — top-most callers → queried symbol → deepest callees — in one connected indented tree per caller chain.

**Architecture:** Two independent BFS passes (upward callers + downward callees) run in parallel via `tokio::join!` in `SymbolContextUseCase`. The results are merged in the controller at render time. Each caller chain is rendered as a self-contained block with the callees subtree hanging off the queried symbol node.

**Tech Stack:** Rust async/await, tokio, serde, clap, existing `CallGraphUseCase` (`find_callers`, `find_callees`), DuckDB in-memory for tests.

---

## Task 1: Replace the data model in `symbol_context.rs`

**Files:**
- Modify: `src/application/use_cases/symbol_context.rs`

The current `ContextEdge` + flat `SymbolContext` struct must be replaced with depth-grouped BFS node structs that mirror `ImpactAnalysis`.

**Step 1: Write a failing test**

Add to `tests/integration_tests.rs` (or a new `tests/symbol_context_tests.rs`):

```rust
// tests/symbol_context_tests.rs
use std::sync::Arc;
use codesearch::{
    CallGraphRepository, CallGraphUseCase, DuckdbCallGraphRepository,
    DuckdbMetadataRepository, DuckdbFileHashRepository, FileHashRepository,
    SymbolContextUseCase, SymbolReference, ReferenceKind,
};

async fn make_call_graph_use_case() -> Arc<CallGraphUseCase> {
    let metadata_repository =
        Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = metadata_repository.shared_connection();
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create call graph repo"),
    );
    Arc::new(CallGraphUseCase::new(call_graph_repo))
}

/// Seed three levels of callers: entry -> middle -> root_symbol
/// and three levels of callees: root_symbol -> child -> grandchild
async fn seed_chain(cg: &Arc<CallGraphUseCase>) {
    let refs = vec![
        // callers chain (upward)
        SymbolReference::new(
            "repo1", "entry", "middle", "src/entry.rs", "src/entry.rs", 1, ReferenceKind::Call, None,
        ),
        SymbolReference::new(
            "repo1", "middle", "root_symbol", "src/middle.rs", "src/middle.rs", 5, ReferenceKind::Call, None,
        ),
        // callees chain (downward)
        SymbolReference::new(
            "repo1", "root_symbol", "child", "src/root.rs", "src/root.rs", 10, ReferenceKind::Call, None,
        ),
        SymbolReference::new(
            "repo1", "child", "grandchild", "src/child.rs", "src/child.rs", 20, ReferenceKind::Call, None,
        ),
    ];
    cg.save_references(&refs).await.expect("Failed to seed references");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_context_callers_by_depth() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;
    let use_case = SymbolContextUseCase::new(cg);
    let ctx = use_case
        .get_context("root_symbol", None, false)
        .await
        .expect("get_context failed");
    // Should have callers at depth 1 (middle) and depth 2 (entry)
    assert_eq!(ctx.callers_by_depth.len(), 2, "expected 2 caller depths");
    assert_eq!(ctx.callers_by_depth[0].len(), 1); // depth 1: middle
    assert_eq!(ctx.callers_by_depth[0][0].symbol, "middle");
    assert_eq!(ctx.callers_by_depth[1].len(), 1); // depth 2: entry
    assert_eq!(ctx.callers_by_depth[1][0].symbol, "entry");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_context_callees_by_depth() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;
    let use_case = SymbolContextUseCase::new(cg);
    let ctx = use_case
        .get_context("root_symbol", None, false)
        .await
        .expect("get_context failed");
    // Should have callees at depth 1 (child) and depth 2 (grandchild)
    assert_eq!(ctx.callees_by_depth.len(), 2, "expected 2 callee depths");
    assert_eq!(ctx.callees_by_depth[0].len(), 1); // depth 1: child
    assert_eq!(ctx.callees_by_depth[0][0].symbol, "child");
    assert_eq!(ctx.callees_by_depth[1].len(), 1); // depth 2: grandchild
    assert_eq!(ctx.callees_by_depth[1][0].symbol, "grandchild");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_context_cycle_guard() {
    let cg = make_call_graph_use_case().await;
    // A calls B, B calls A (cycle in callees)
    let refs = vec![
        SymbolReference::new(
            "repo1", "A", "B", "src/a.rs", "src/a.rs", 1, ReferenceKind::Call, None,
        ),
        SymbolReference::new(
            "repo1", "B", "A", "src/b.rs", "src/b.rs", 2, ReferenceKind::Call, None,
        ),
    ];
    cg.save_references(&refs).await.expect("seed failed");
    let use_case = SymbolContextUseCase::new(cg);
    // Should not infinite loop
    let ctx = use_case
        .get_context("A", None, false)
        .await
        .expect("get_context must not loop");
    assert!(ctx.total_callees > 0);
}
```

**Step 2: Run to confirm they fail**

```bash
cargo test --test symbol_context_tests 2>&1 | head -30
```

Expected: compile errors / test failures (the new struct fields don't exist yet).

**Step 3: Replace `ContextEdge` / `SymbolContext` in `symbol_context.rs`**

Replace the entire content of `src/application/use_cases/symbol_context.rs` with the new implementation below. Key changes:

- Remove `ContextEdge` struct.
- Add `ContextNode` struct (mirrors `ImpactNode`).
- Replace `SymbolContext` with depth-grouped fields.
- Replace one-hop `get_context` logic with two parallel BFS passes.

New `ContextNode`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextNode {
    pub symbol: String,
    pub depth: usize,
    pub file_path: String,
    pub line: u32,
    pub reference_kind: String,
    pub repository_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_symbol: Option<String>,
}
```

New `SymbolContext`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolContext {
    pub symbol: String,
    pub root_symbols: Vec<String>,
    /// Callers BFS: index 0 = depth 1 = direct callers.
    pub callers_by_depth: Vec<Vec<ContextNode>>,
    pub total_callers: usize,
    pub max_caller_depth: usize,
    /// Callees BFS: index 0 = depth 1 = direct callees.
    pub callees_by_depth: Vec<Vec<ContextNode>>,
    pub total_callees: usize,
    pub max_callee_depth: usize,
}
```

New `get_context` signature (remove `limit` parameter — it was `Option<u32>`):
```rust
pub async fn get_context(
    &self,
    symbol: &str,
    repository_id: Option<&str>,
    is_regex: bool,
) -> Result<SymbolContext, DomainError>
```

BFS logic for callees (downward) mirrors impact's upward BFS exactly — use `find_callees` instead of `find_callers`, and track `callee_symbol` instead of `caller_symbol`.

Upward BFS (callers) should call the same logic as `ImpactAnalysisUseCase::analyze` — copy and simplify it inline (no `depth` cap, no `max_depth` parameter needed).

Both BFS passes run after symbol resolution:
```rust
let (callers_result, callees_result) = tokio::join!(
    self.run_callers_bfs(&root_symbols, &query),
    self.run_callees_bfs(&root_symbols, &query),
);
let callers_by_depth = callers_result?;
let callees_by_depth = callees_result?;
```

**Step 4: Run tests**

```bash
cargo test --test symbol_context_tests 2>&1
```

Expected: all 3 tests pass.

**Step 5: Commit**

```bash
git add src/application/use_cases/symbol_context.rs tests/symbol_context_tests.rs
git commit -m "feat: replace ContextEdge/SymbolContext with depth-grouped BFS model"
```

---

## Task 2: Fix all compilation errors from the model change

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/application/use_cases/mod.rs` (if it re-exports `ContextEdge`)
- Modify: `src/connector/api/controller/symbol_context_controller.rs`
- Modify: `src/cli/mod.rs`
- Modify: `src/connector/api/router.rs`
- Modify: `src/connector/adapter/mcp/` (if it references `ContextEdge` or the old struct)

**Step 1: Run the build to see all errors**

```bash
cargo build 2>&1
```

**Step 2: Fix `src/lib.rs`**

Remove `ContextEdge` from the `pub use application::{...}` line. Add `ContextNode` in its place:

```rust
pub use application::{
    CallGraphQuery, CallGraphRepository, CallGraphStats, CallGraphUseCase, ChatClient, ContextNode,
    DeleteRepositoryUseCase, EmbeddingService, ExplainResult, ExplainUseCase, FileHashRepository,
    ImpactAnalysis, ImpactAnalysisUseCase, ImpactNode, IndexRepositoryUseCase,
    ListRepositoriesUseCase, MetadataRepository, ParserService, QueryExpander, RerankingService,
    Scip, SearchCodeUseCase, SnippetLookupUseCase, SymbolContext, SymbolContextUseCase,
    VectorRepository,
};
```

**Step 3: Fix `src/application/use_cases/mod.rs`**

Check what it re-exports. Replace any `ContextEdge` with `ContextNode`.

**Step 4: Fix `src/cli/mod.rs`**

Remove the `--limit` flag from the `Context` command variant:

Before:
```rust
Context {
    symbol: String,
    #[arg(short, long)]
    repository: Option<String>,
    #[arg(short, long)]
    limit: Option<u32>,
    #[arg(short = 'F', long, value_enum, default_value = "text")]
    format: OutputFormat,
    #[arg(long)]
    regex: bool,
},
```

After:
```rust
/// Show full end-to-end call chain tree for a symbol: callers BFS (top-most
/// entry points → symbol) and callees BFS (symbol → deepest callees), merged
/// into one contiguous indented tree per caller chain.
Context {
    /// Symbol name to look up (e.g. "authenticate" or "MyStruct::new").
    /// When --regex is set, treated as a POSIX regular expression matched
    /// against all indexed fully-qualified symbol names.
    symbol: String,

    /// Restrict context to a specific repository ID
    #[arg(short, long)]
    repository: Option<String>,

    /// Output format: text, json, or vimgrep
    #[arg(short = 'F', long, value_enum, default_value = "text")]
    format: OutputFormat,

    /// Use SYMBOL as an explicit regex pattern without auto-wrapping.
    #[arg(long)]
    regex: bool,
},
```

**Step 5: Fix `src/connector/api/router.rs`**

Remove `limit` from the `Context` arm:

Before:
```rust
Commands::Context {
    symbol,
    repository,
    limit,
    format,
    regex,
} => {
    self.symbol_context_controller
        .context(symbol, repository, limit, format, regex)
        .await
}
```

After:
```rust
Commands::Context {
    symbol,
    repository,
    format,
    regex,
} => {
    self.symbol_context_controller
        .context(symbol, repository, format, regex)
        .await
}
```

**Step 6: Run build again to confirm only the controller remains broken**

```bash
cargo build 2>&1
```

**Step 7: Commit partial fix**

```bash
git add src/lib.rs src/application/use_cases/mod.rs src/cli/mod.rs src/connector/api/router.rs
git commit -m "fix: update exports and CLI after SymbolContext model change"
```

---

## Task 3: Rewrite `SymbolContextController` to render the new tree

**Files:**
- Modify: `src/connector/api/controller/symbol_context_controller.rs`

This is the most complex rendering task. The controller must:

1. Accept the new `SymbolContext` (no `limit` parameter).
2. For text format: render each caller-chain block as a self-contained tree from top-most caller down to the queried symbol, then continue with the callees subtree hanging off the queried symbol.
3. For vimgrep: one line per node with `←` for callers, `→` for callees.
4. For JSON: unchanged (`serde_json::to_string_pretty(&ctx)`).

**Step 1: Understand the rendering algorithm**

For text rendering, the algorithm mirrors `ImpactController::format_impact`:

**Callers side:**
- Collect all nodes from `callers_by_depth` into a flat vec.
- Build `children_map`: `via_symbol → [nodes that list it as via_symbol]`.
- Find leaf nodes (not in any `via_symbol`) — these are the top-most entry points.
- For each leaf, trace back via `via_symbol` to build the path from leaf → queried symbol (same as `render_reversed_path` in `ImpactController`).
- The path is rendered with the queried symbol as the terminal node.

**Callees side:**
- Build the callees tree rooted at the queried symbol.
- Recursively render `callees_by_depth` as children of the queried symbol using `via_symbol` linkage.
- The callees rendering recurses from depth 1 downward.

**Junction:**
- Each caller chain block renders as:
  ```
  [caller chain from top to queried symbol]
  └── [queried symbol]
      ├── [callee at depth 1 A]
      │   └── [callee at depth 2 A.1]
      └── [callee at depth 1 B]
  ```
- When there are no callers, render just the callees tree rooted at the symbol.
- When there are no callees, degrade to impact-style callers-only output.

**Step 2: Write the new controller**

Key methods to implement:

```rust
// Collect all callee nodes indexed by (depth, symbol) for tree traversal
fn build_callee_children_map<'a>(
    ctx: &'a SymbolContext,
) -> HashMap<&'a str, Vec<&'a ContextNode>>

// Render the callees subtree starting from the queried symbol at a given indent depth
fn render_callees_subtree(
    symbol: &str,
    callee_children: &HashMap<&str, Vec<&ContextNode>>,
    indent_depth: usize,
    out: &mut String,
)

// For each caller-chain leaf: render the chain top-down, then attach callees
fn render_caller_chain_with_callees(
    path: &[&ContextNode],        // leaf-first (top-most caller at [0])
    root_symbol: &str,
    callee_children: &HashMap<&str, Vec<&ContextNode>>,
    out: &mut String,
)
```

The `render_callees_subtree` function must handle cycles (track visited set) to guard against the same node appearing multiple times at different depths.

Full updated controller:

```rust
use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;

use crate::cli::OutputFormat;
use crate::application::use_cases::symbol_context::{ContextNode, SymbolContext};

use super::super::Container;

pub struct SymbolContextController<'a> {
    container: &'a Container,
}

impl<'a> SymbolContextController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn context(
        &self,
        symbol: String,
        repository: Option<String>,
        format: OutputFormat,
        is_regex: bool,
    ) -> Result<String> {
        let use_case = self.container.context_use_case();
        let ctx = use_case
            .get_context(&symbol, repository.as_deref(), is_regex)
            .await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&ctx)?,
            OutputFormat::Vimgrep => Self::format_vimgrep(&ctx),
            OutputFormat::Text => Self::format_text(&ctx),
        })
    }

    fn format_vimgrep(ctx: &SymbolContext) -> String {
        let callers = ctx.callers_by_depth.iter().flatten().map(|n| {
            format!("{}:{}:1:← {} [{}]", n.file_path, n.line, n.symbol, n.reference_kind)
        });
        let callees = ctx.callees_by_depth.iter().flatten().map(|n| {
            format!("{}:{}:1:→ {} [{}]", n.file_path, n.line, n.symbol, n.reference_kind)
        });
        callers.chain(callees).collect::<Vec<_>>().join("\n")
    }

    fn format_text(ctx: &SymbolContext) -> String {
        let mut out = format!(
            "Context for '{}'\n\
             ─────────────────────────────────────────\n",
            ctx.symbol
        );

        let has_callers = ctx.total_callers > 0;
        let has_callees = ctx.total_callees > 0;

        // Build callee children map once, reused per caller chain
        let callee_children = Self::build_callee_children_map(ctx);

        if !has_callers && !has_callees {
            out.push_str("No callers or callees found for this symbol.\n");
            return out;
        }

        if has_callers {
            // Build caller tree structures
            let all_callers: Vec<&ContextNode> = ctx.callers_by_depth.iter().flatten().collect();
            let mut caller_children: HashMap<&str, Vec<&ContextNode>> = HashMap::new();
            for node in &all_callers {
                if let Some(via) = node.via_symbol.as_deref() {
                    caller_children.entry(via).or_default().push(node);
                }
            }

            // Leaf = top-most entry-point (no one lists this symbol as its via_symbol)
            let leaf_nodes: Vec<&ContextNode> = all_callers
                .iter()
                .copied()
                .filter(|n| !caller_children.contains_key(n.symbol.as_str()))
                .collect();

            // Lookup map for path tracing
            let mut node_by_depth_sym: HashMap<(usize, &str), &ContextNode> = HashMap::new();
            for node in &all_callers {
                node_by_depth_sym
                    .entry((node.depth, node.symbol.as_str()))
                    .or_insert(node);
            }

            for (idx, &leaf) in leaf_nodes.iter().enumerate() {
                // Build path from leaf → queried symbol
                let mut path: Vec<&ContextNode> = vec![leaf];
                let mut current = leaf;
                while let Some(via) = current.via_symbol.as_deref() {
                    let parent_depth = current.depth.saturating_sub(1);
                    if let Some(&parent) = node_by_depth_sym.get(&(parent_depth, via)) {
                        path.push(parent);
                        current = parent;
                    } else {
                        break;
                    }
                }
                // path[0] = leaf (top-most), path[last] = direct caller of queried symbol
                Self::render_chain(&path, &ctx.symbol, &callee_children, &mut out);
                if idx < leaf_nodes.len() - 1 {
                    out.push('\n');
                }
            }
        } else {
            // No callers: render callees subtree rooted at the symbol directly
            out.push_str(&format!("{}\n", ctx.symbol));
            let mut visited = HashSet::new();
            Self::render_callees_subtree(
                &ctx.symbol,
                &callee_children,
                0,
                &mut out,
                &mut visited,
            );
        }

        out
    }

    fn build_callee_children_map(ctx: &SymbolContext) -> HashMap<String, Vec<&ContextNode>> {
        let mut map: HashMap<String, Vec<&ContextNode>> = HashMap::new();
        for node in ctx.callees_by_depth.iter().flatten() {
            let key = node.via_symbol.clone().unwrap_or_else(|| ctx.symbol.clone());
            map.entry(key).or_default().push(node);
        }
        map
    }

    /// Render caller path (top-most entry → direct caller) then queried symbol with callees.
    fn render_chain(
        path: &[&ContextNode],
        root_symbol: &str,
        callee_children: &HashMap<String, Vec<&ContextNode>>,
        out: &mut String,
    ) {
        if path.is_empty() {
            return;
        }
        // path[0] is the leaf (top-most caller), rendered at indent 0
        for (depth, node) in path.iter().enumerate() {
            let alias = node.import_alias.as_ref()
                .map(|a| format!(", as {}", a))
                .unwrap_or_default();
            if depth == 0 {
                out.push_str(&format!(
                    "{} [{}{}]  {}:{}\n",
                    node.symbol, node.reference_kind, alias, node.file_path, node.line,
                ));
            } else {
                let indent = "    ".repeat(depth - 1);
                out.push_str(&format!(
                    "{}└── {} [{}{}]  {}:{}\n",
                    indent, node.symbol, node.reference_kind, alias, node.file_path, node.line,
                ));
            }
        }
        // The queried symbol is the terminal node in the caller chain
        let caller_indent = "    ".repeat(path.len() - 1);
        out.push_str(&format!("{}└── {}\n", caller_indent, root_symbol));

        // Now render callees hanging off the queried symbol at caller_indent + 4
        let callee_base_depth = path.len(); // indent level for depth-1 callees
        let mut visited = HashSet::new();
        Self::render_callees_subtree(root_symbol, callee_children, callee_base_depth, out, &mut visited);
    }

    /// Recursively render the callees subtree rooted at `parent_symbol`.
    fn render_callees_subtree(
        parent_symbol: &str,
        callee_children: &HashMap<String, Vec<&ContextNode>>,
        indent_depth: usize,
        out: &mut String,
        visited: &mut HashSet<String>,
    ) {
        let children = match callee_children.get(parent_symbol) {
            Some(c) => c,
            None => return,
        };
        let count = children.len();
        for (i, node) in children.iter().enumerate() {
            if !visited.insert(node.symbol.clone()) {
                continue; // cycle guard
            }
            let alias = node.import_alias.as_ref()
                .map(|a| format!(", as {}", a))
                .unwrap_or_default();
            let is_last = i == count - 1;
            let branch = if is_last { "└──" } else { "├──" };
            let indent = "    ".repeat(indent_depth);
            out.push_str(&format!(
                "{}{} {} [{}{}]  {}:{}\n",
                indent, branch, node.symbol, node.reference_kind, alias, node.file_path, node.line,
            ));
            // Recurse
            Self::render_callees_subtree(
                &node.symbol,
                callee_children,
                indent_depth + 1,
                out,
                visited,
            );
        }
    }
}
```

**Step 3: Run the build**

```bash
cargo build 2>&1
```

Expected: clean build.

**Step 4: Run all tests**

```bash
cargo test 2>&1
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add src/connector/api/controller/symbol_context_controller.rs
git commit -m "feat: rewrite SymbolContextController to render full end-to-end call chain tree"
```

---

## Task 4: Check MCP adapter for any references to old `ContextEdge` / `limit`

**Files:**
- Scan: `src/connector/adapter/mcp/`

**Step 1: Search for references**

```bash
grep -rn "ContextEdge\|context.*limit\|get_context" src/connector/adapter/mcp/
```

**Step 2: Fix any broken references**

If the MCP server passes a `limit` argument to `get_context`, remove it. If it serializes `ContextEdge`, update to `ContextNode`.

**Step 3: Run build + tests**

```bash
cargo build 2>&1 && cargo test 2>&1
```

**Step 4: Commit if changes were made**

```bash
git add src/connector/adapter/mcp/
git commit -m "fix: update MCP adapter after context command revamp"
```

---

## Task 5: Check TUI for any references to old model

**Files:**
- Scan: `src/tui/`

**Step 1: Search for references**

```bash
grep -rn "ContextEdge\|SymbolContext\|context_use_case\|get_context" src/tui/
```

**Step 2: Fix any broken references**

Update field accesses: `ctx.callers` → iterate `ctx.callers_by_depth`, `ctx.callees` → iterate `ctx.callees_by_depth`, `ctx.caller_count` → `ctx.total_callers`, `ctx.callee_count` → `ctx.total_callees`.

**Step 3: Run build + tests**

```bash
cargo build 2>&1 && cargo test 2>&1
```

**Step 4: Commit if changes were made**

```bash
git add src/tui/
git commit -m "fix: update TUI after context command revamp"
```

---

## Task 6: Final verification

**Step 1: Run the full test suite**

```bash
cargo test 2>&1
```

Expected: all tests pass, no warnings that Clippy would flag.

**Step 2: Run Clippy**

```bash
cargo clippy 2>&1
```

Fix any warnings.

**Step 3: Run fmt**

```bash
cargo fmt && cargo fmt --check 2>&1
```

**Step 4: Final commit (fmt/clippy fixes if any)**

```bash
git add -A
git commit -m "chore: apply fmt and clippy fixes after context revamp"
```
