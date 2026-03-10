# Context Command Revamp — Design Document

**Date:** 2026-03-10  
**Status:** Approved

---

## Problem

The current `context` command shows a flat one-hop view: a list of direct callers and a list of direct callees. This gives only a shallow, disconnected picture of a symbol's place in the call graph. The `impact` command already does a multi-hop BFS upward (callers of callers…) and renders a rich call-chain tree, but it only covers one direction.

## Goal

Revamp `context` to show a **full end-to-end call chain tree**: top-most entry-point callers → … → queried symbol → … → deepest callees — all in one connected, indented tree per caller chain.

`impact` remains unchanged. `context` becomes the bidirectional, full-depth view.

---

## Approach

Two independent BFS passes (Approach A), merged at render time in the controller.

- **Upward BFS** (callers): logically identical to `ImpactAnalysisUseCase::analyze`. Traverse `find_callers` hop by hop until no new symbols are found.
- **Downward BFS** (callees): symmetric — traverse `find_callees` hop by hop until no new symbols are found.
- Both BFS passes run in parallel via `tokio::join!` after symbol resolution.
- Unlimited depth for both directions (no `--depth` or `--limit` flag in the revamped command).
- Symbol resolution (exact → fuzzy auto-wrap fallback) is unchanged.

---

## Data Model Changes

### `SymbolContext` (replaces existing struct)

```rust
/// A single node in a BFS traversal tree (callers or callees direction).
pub struct ContextNode {
    pub symbol: String,
    pub depth: usize,           // hops from the queried symbol
    pub file_path: String,
    pub line: u32,
    pub reference_kind: String,
    pub repository_id: String,
    pub import_alias: Option<String>,
    pub via_symbol: Option<String>,  // parent in the BFS tree
}

pub struct SymbolContext {
    /// Display label for the queried symbol (may include "(N symbols)" suffix).
    pub symbol: String,
    /// Resolved fully-qualified symbol names used as BFS roots.
    pub root_symbols: Vec<String>,

    // Callers BFS (upward): index 0 = depth 1 = direct callers.
    pub callers_by_depth: Vec<Vec<ContextNode>>,
    pub total_callers: usize,
    pub max_caller_depth: usize,

    // Callees BFS (downward): index 0 = depth 1 = direct callees.
    pub callees_by_depth: Vec<Vec<ContextNode>>,
    pub total_callees: usize,
    pub max_callee_depth: usize,
}
```

`ContextEdge` is removed. The JSON output shape changes; this is acceptable as part of the revamp.

---

## CLI Changes

The `--limit` flag on the `Context` command is removed (unlimited BFS depth). All other flags (`--repository`, `--format`, `--regex`) are unchanged.

---

## Rendering (Text Format)

Each caller chain (leaf entry-point → queried symbol) is rendered as a fully self-contained block. The queried symbol appears at the junction, with the callees subtree hanging off it as children.

### Example output

```
context for 'authenticate'
──────────────────────────────────────────

EntryPointA [call]  src/main.rs:10
└── MiddleLayer [call]  src/auth/mod.rs:42
    └── authenticate
        ├── validate_token [call]  src/token.rs:7
        │   └── decode_jwt [call]  src/jwt.rs:22
        └── fetch_user [call]  src/db/users.rs:88
            └── run_query [call]  src/db/mod.rs:55

EntryPointB [call]  src/api/routes.rs:99
└── authenticate
    ├── validate_token [call]  src/token.rs:7
    └── fetch_user [call]  src/db/users.rs:88
```

### Degenerate cases

- **No callers** (root entry-point): render the callees tree rooted at the symbol directly.
- **No callees** (leaf symbol): degrades to the impact view — caller chains only.
- **Neither**: print a "no references found" message.

### Vimgrep format

One line per node; direction prefix `←` for callers, `→` for callees:

```
src/main.rs:10:1:← EntryPointA [call]
src/token.rs:7:1:→ validate_token [call]
```

---

## Files to Change

| File | Change |
|------|--------|
| `src/application/use_cases/symbol_context.rs` | Replace `ContextEdge`/`SymbolContext` with `ContextNode`/`SymbolContext`; replace one-hop logic with bidirectional BFS |
| `src/cli/mod.rs` | Remove `--limit` from `Context` command |
| `src/connector/api/controller/symbol_context_controller.rs` | Rewrite `format_context` and `format_context_vimgrep` for new data model and tree rendering |
| `src/connector/api/container.rs` | No change expected (use case constructor signature unchanged) |
| `src/connector/adapter/mcp/` | Update MCP tool description/output if it calls the context use case |
| `tests/integration_tests.rs` | Update / add tests for bidirectional BFS context |

---

## Testing

- Add an integration test that indexes a small fixture with a 3-level call chain and verifies:
  - `callers_by_depth` contains nodes at the correct depths
  - `callees_by_depth` contains nodes at the correct depths
  - Cycle guard prevents infinite loops when A→B→A cycles exist
- Existing impact tests are unaffected.
