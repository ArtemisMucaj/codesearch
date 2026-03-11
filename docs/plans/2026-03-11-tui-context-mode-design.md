# TUI Context Mode — Design Document

**Date:** 2026-03-11  
**Status:** Approved

---

## Goal

Add a **Context** mode to the TUI as a third tab alongside Search and Impact. It lets the user type a symbol name, run `SymbolContextUseCase::get_context`, and view the full bidirectional call chain (callers up to entry points → queried symbol → deepest callees) interactively.

---

## Layout

```
┌ Search   Impact  [Context] ──────────────────────────┐
│ > my_function                                         │
├───────────────────────┬───────────────────────────────┤
│ Entry-points (3)      │ Call context tree             │
│ ───────────────────── │ ─────────────────────────     │
│ ▶ src/main.rs:12      │ ★  entry_pt  src/main.rs:12  │
│   src/lib.rs:45       │    │                          │
│   src/api.rs:80       │    └── ▶ middleware  lib:45   │
│                       │           │                   │
│                       │           └── ◉  my_function  │
│                       │               ├── child_A     │
│                       │               └── child_B     │
│                       │                   └── grand   │
└───────────────────────┴───────────────────────────────┘
 Enter: analyse  ↑↓: navigate  Ctrl+→: focus tree …
```

**Left pane (35%):** List of entry-point callers — the leaf nodes from the callers BFS. Each row shows `file:line` with the short symbol as a sub-label. When there are no callers, a single synthetic entry ("No callers — callees only") is shown to allow the right pane to render.

**Right pane (65%):** Full call-chain tree for the selected entry-point. The path is rendered top-down (entry-point → queried symbol) using the same `★ / └──` style as the Impact view, with the queried symbol marked `◉` in red. Below the `◉` node the callees subtree hangs with `├──`/`└──`/`│` connectors. Nodes in the callers chain are individually selectable (↑↓ when tree pane focused); pressing Enter on a selected node loads its code snippet.

---

## State

### New types in `src/tui/state.rs`

```rust
/// Which pane in the context view has keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ContextPane {
    #[default]
    EntryPoints,
    Tree,
}

#[derive(Debug, Default)]
pub struct ContextState {
    pub input: String,
    pub cursor: usize,
    pub context: Option<SymbolContext>,
    /// Selected row in the entry-points list (left pane).
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Vertical scroll offset for the tree pane.
    pub tree_scroll: u16,
    pub repository: Option<String>,
    pub pending_key: Option<String>,
    pub errored_key: Option<String>,
    pub focused_pane: ContextPane,
    /// Selected node index within the call chain (tree pane, callers side only).
    pub chain_selected: usize,
    /// Code snippet for the selected chain node.
    pub chain_snippet: Option<CodeChunk>,
    pub chain_snippet_loading: bool,
    pub chain_snippet_scroll: u16,
    pub chain_snippet_pending_key: Option<SnippetKey>,
}
```

`ActiveMode` gains a third variant:
```rust
pub enum ActiveMode { Search, Impact, Context }
```

`AppState` gains a `context: ContextState` field.

The `active_input`, `active_input_mut`, `active_cursor`, `active_cursor_mut`, and `is_loading` methods are extended with `ActiveMode::Context` arms.

---

## Events

Two new variants in `src/tui/event.rs`:

```rust
ContextDone {
    key: String,
    result: Result<SymbolContext, String>,
},
ContextSnippetDone {
    key: SnippetKey,
    result: Result<Option<CodeChunk>, String>,
},
```

---

## Cache

`TuiCache` gains:
```rust
pub contexts: HashMap<String, SymbolContext>,
```

And a helper:
```rust
pub fn context_key(symbol: &str, repository: Option<&str>) -> String { … }
```

---

## App (`src/tui/app.rs`)

- `TuiApp` gains a `context_uc: Option<Arc<SymbolContextUseCase>>` field and a `context_task: Option<JoinHandle<()>>`.
- `ContainerReady` handler sets `self.context_uc = Some(Arc::new(container.context_use_case()))`.
- Tab cycling: `Search → Impact → Context → Search`.
- `Ctrl+↑` in Search jumps to Impact (unchanged).
- `Ctrl+↓` in Search jumps to Context with the selected symbol pre-filled.
- `dispatch_context()` mirrors `dispatch_impact()`: cache-first, key dedup, spawns async task.
- `dispatch_context_snippet()` mirrors `dispatch_chain_snippet()`: extracts `(repo_id, file_path, line)` from the selected chain node in the callers path.
- `handle_app_event` handles `ContextDone` and `ContextSnippetDone`.
- Key handling for Context: same as Impact but using `ContextPane::EntryPoints / Tree`.

Typing in the tree pane resets focus to `EntryPoints` (same guard as Impact).

---

## View (`src/tui/views/context.rs`)

New file. Two render functions:

**`render_entry_points(frame, area, state)`** — mirrors `impact::render_entry_points`. Builds the `ListEntry` list from `ctx.callers_by_depth` leaf nodes (extracted via the same path-tracing logic used by the controller). When no callers, shows one synthetic entry for the symbol itself.

**`render_right(frame, area, state)`** — mirrors `impact::render_right`:
- Loading / empty states.
- Code snippet view (when `chain_snippet` is loaded).
- Default: calls `render_call_context_tree(frame, area, path, root_symbol, callee_children, …)`.

**`render_call_context_tree`** — extends `impact::render_path_tree`:
1. Renders the caller chain (entry-point → direct caller → `◉ root_symbol`) using the same `★ / └──` style, with node selection.
2. Below the `◉` row, renders the callee subtree using `├──`/`└──`/`│` lines (mirroring the controller's text renderer). Callee nodes are **not** selectable (no Enter-to-code for callees in v1 — keeps scope minimal).

---

## Input Bar (`src/tui/widgets/input_bar.rs`)

Three tabs instead of two:
```
 Search   Impact   Context
```
The active tab is highlighted; inactive tabs are dimmed.

---

## CLI (`src/cli/mod.rs`)

`TuiMode` gains:
```rust
/// Open in context mode.
Context,
```

---

## `src/main.rs`

The `Commands::Tui` loading path passes `mode` to `TuiApp::new_loading`; the new `TuiMode::Context` variant maps to `ActiveMode::Context`. No other changes needed.

---

## Jump shortcut

In `src/tui/app.rs`, the `Ctrl+↓` key in Search mode:
1. Extracts the selected result's `qualified_name` or `symbol_name`.
2. Populates `state.context.input` and `cursor`.
3. Resets context state.
4. Sets `state.mode = ActiveMode::Context`.
5. Calls `dispatch_context()`.

---

## Entry-point list construction

The left pane needs a flat list of entry-point nodes from the callers BFS. The use-case already returns `callers_by_depth`. Leaf nodes (those not pointed to as `via_symbol` by any other node) are the entry-points — the same logic used in the controller's `format_text`. This is extracted into a small helper `leaf_caller_nodes(ctx: &SymbolContext) -> Vec<&ContextNode>` in `views/context.rs`.

---

## Deferred / Out of scope

- `--mode context` auto-dispatch on startup (follow-on).
- Callee node code-snippet lookup on Enter (follow-on; callee nodes are not selectable in v1).
- Ctrl+↑ in Context to jump to Impact (follow-on).
