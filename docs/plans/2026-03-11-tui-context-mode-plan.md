# TUI Context Mode Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a Context tab to the TUI (third tab after Search and Impact) that lets users type a symbol name, run `SymbolContextUseCase::get_context`, and view the full bidirectional call-chain tree interactively.

**Architecture:** Follows the exact same pattern as the existing Impact mode. New `ContextState`/`ContextPane` types mirror `ImpactState`/`ImpactPane`. New `views/context.rs` mirrors `views/impact.rs`. All async dispatch, event handling, and caching patterns are copied from Impact and adapted for `SymbolContext`.

**Tech Stack:** Rust, ratatui, tokio, existing `SymbolContextUseCase` (already wired in `Container::context_use_case()`), `ContextNode`/`SymbolContext` domain types.

**Design doc:** `docs/plans/2026-03-11-tui-context-mode-design.md`

---

## Reference: Key Existing Files

Before implementing, always read these files for exact patterns to follow:

| File | Why you need it |
|---|---|
| `src/tui/state.rs` | Add `ContextState`, `ContextPane`, `ActiveMode::Context` here |
| `src/tui/event.rs` | Add `ContextDone`, `ContextSnippetDone` here |
| `src/tui/cache.rs` | Add `contexts` map + `context_key()` here |
| `src/tui/app.rs` | Add `context_uc`, dispatch, handle, key handling here |
| `src/tui/views/mod.rs` | Route `ActiveMode::Context`, update status bar hints |
| `src/tui/views/impact.rs` | **Primary reference** — copy and adapt for context |
| `src/tui/widgets/input_bar.rs` | Add third tab |
| `src/cli/mod.rs` | Add `Context` to `TuiMode` |
| `src/main.rs` | Handle `TuiMode::Context` in loading path |
| `src/connector/api/controller/symbol_context_controller.rs` | Port entry-point extraction + path tracing + callee subtree rendering |
| `src/application/use_cases/symbol_context.rs` | `SymbolContext`, `ContextNode` data model |

---

## Task 1: State types (`src/tui/state.rs`)

**Files:**
- Modify: `src/tui/state.rs`

**Step 1: Add imports at the top of the file**

Add `SymbolContext` to the existing import from `crate`. The current imports are:
```rust
use crate::application::ImpactAnalysis;
use crate::domain::{CodeChunk, SearchResult};
use crate::tui::cache::SnippetKey;
```

Add:
```rust
use crate::application::use_cases::symbol_context::SymbolContext;
```

**Step 2: Add `ActiveMode::Context` variant**

Find and replace:
```rust
pub enum ActiveMode {
    Search,
    Impact,
}
```
With:
```rust
pub enum ActiveMode {
    Search,
    Impact,
    Context,
}
```

**Step 3: Add `ContextPane` enum** (after the `ImpactPane` definition)

```rust
/// Which pane in the context view has keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ContextPane {
    #[default]
    EntryPoints,
    Tree,
}
```

**Step 4: Add `ContextState` struct** (after the `ImpactState` definition)

```rust
#[derive(Debug, Default)]
pub struct ContextState {
    pub input: String,
    /// Cursor position within `input`, measured in characters (not bytes).
    pub cursor: usize,
    pub context: Option<SymbolContext>,
    /// Index into the entry-point list shown in the left pane.
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Vertical scroll offset for the tree pane (right pane).
    pub tree_scroll: u16,
    /// Optional repository filter forwarded to the use case.
    pub repository: Option<String>,
    /// Cache key of the most recently dispatched context request.
    pub pending_key: Option<String>,
    /// Cache key of the last request that returned an error.
    pub errored_key: Option<String>,
    /// Which pane currently has keyboard focus.
    pub focused_pane: ContextPane,
    /// Selected node index within the current call chain (tree pane, callers side only).
    pub chain_selected: usize,
    /// Code snippet for the selected chain node (Some = code view is active).
    pub chain_snippet: Option<CodeChunk>,
    pub chain_snippet_loading: bool,
    /// Vertical scroll offset for the chain code view.
    pub chain_snippet_scroll: u16,
    /// Pending key for an in-flight chain snippet request.
    pub chain_snippet_pending_key: Option<SnippetKey>,
}
```

**Step 5: Add `context: ContextState` field to `AppState`**

Find:
```rust
pub struct AppState {
    pub mode: ActiveMode,
    pub search: SearchState,
    pub impact: ImpactState,
    pub should_quit: bool,
```
Replace with:
```rust
pub struct AppState {
    pub mode: ActiveMode,
    pub search: SearchState,
    pub impact: ImpactState,
    pub context: ContextState,
    pub should_quit: bool,
```

**Step 6: Initialise `context` in `AppState::new`**

Find the `Self { ... }` block in `AppState::new`. There are two fields to add:
- In the struct literal, after `impact: ImpactState { repository, ..Default::default() },` add:
  ```rust
  context: ContextState {
      repository: repository.clone(),
      ..Default::default()
  },
  ```
  
  Note: `repository` is moved into `impact`. Change the `impact` line to clone first:
  ```rust
  impact: ImpactState {
      repository: repository.clone(),
      ..Default::default()
  },
  context: ContextState {
      repository,
      ..Default::default()
  },
  ```

  And remove the original `repository` binding from before so the borrow checker is happy. The full `new` function body should look like:

  ```rust
  pub fn new(
      repository: Option<String>,
      initial_mode: ActiveMode,
      initial_query: Option<String>,
      models_ready: bool,
  ) -> Self {
      let mut state = Self {
          mode: initial_mode.clone(),
          search: SearchState {
              repository: repository.clone(),
              ..Default::default()
          },
          impact: ImpactState {
              repository: repository.clone(),
              ..Default::default()
          },
          context: ContextState {
              repository,
              ..Default::default()
          },
          should_quit: false,
          models_ready,
      };
      if let Some(query) = initial_query {
          match initial_mode {
              ActiveMode::Search => {
                  state.search.cursor = query.chars().count();
                  state.search.input = query;
              }
              ActiveMode::Impact => {
                  state.impact.cursor = query.chars().count();
                  state.impact.input = query;
              }
              ActiveMode::Context => {
                  state.context.cursor = query.chars().count();
                  state.context.input = query;
              }
          }
      }
      state
  }
  ```

**Step 7: Extend `active_input`, `active_input_mut`, `active_cursor`, `active_cursor_mut`, `is_loading` with `Context` arms**

For each match in the file, add:
- `active_input`: `ActiveMode::Context => &self.context.input,`
- `active_input_mut`: `ActiveMode::Context => &mut self.context.input,`
- `active_cursor`: `ActiveMode::Context => self.context.cursor,`
- `active_cursor_mut`: `ActiveMode::Context => &mut self.context.cursor,`
- `is_loading`: `ActiveMode::Context => self.context.loading,`

**Step 8: Build and check for compile errors**

```bash
cargo build 2>&1 | head -50
```
Expected: compile errors about exhaustive matches elsewhere (in `app.rs`, `views/mod.rs`, etc.) — that is fine, those will be fixed in later tasks. What must NOT appear: errors within `state.rs` itself.

**Step 9: Commit**

```bash
git add src/tui/state.rs
git commit -m "feat(tui): add ContextState, ContextPane, ActiveMode::Context to AppState"
```

---

## Task 2: Events and Cache

**Files:**
- Modify: `src/tui/event.rs`
- Modify: `src/tui/cache.rs`

### event.rs

**Step 1: Add imports**

Add `SymbolContext` to the existing imports:
```rust
use crate::application::use_cases::symbol_context::SymbolContext;
```

**Step 2: Add two new `TuiEvent` variants** (after `ChainSnippetDone`)

```rust
/// Symbol context use case completed.
ContextDone {
    key: String,
    result: Result<SymbolContext, String>,
},
/// Snippet lookup for a selected context chain node completed.
ContextSnippetDone {
    key: SnippetKey,
    result: Result<Option<CodeChunk>, String>,
},
```

### cache.rs

**Step 1: Add import**

Add to imports:
```rust
use crate::application::use_cases::symbol_context::SymbolContext;
```

**Step 2: Add `contexts` field to `TuiCache`**

Find:
```rust
pub struct TuiCache {
    pub searches: HashMap<String, Vec<SearchResult>>,
    pub impacts: HashMap<String, ImpactAnalysis>,
    pub snippets: HashMap<SnippetKey, Option<CodeChunk>>,
}
```
Replace with:
```rust
pub struct TuiCache {
    pub searches: HashMap<String, Vec<SearchResult>>,
    pub impacts: HashMap<String, ImpactAnalysis>,
    pub contexts: HashMap<String, SymbolContext>,
    pub snippets: HashMap<SnippetKey, Option<CodeChunk>>,
}
```

**Step 3: Add `context_key` helper** (after the `impact_key` method)

```rust
/// Build the cache key for a context lookup.
pub fn context_key(symbol: &str, repository: Option<&str>) -> String {
    serde_json::to_string(&(symbol, repository.unwrap_or("")))
        .expect("serde_json serialisation of (&str, &str) is infallible")
}
```

**Step 4: Build check**

```bash
cargo build 2>&1 | head -50
```

**Step 5: Commit**

```bash
git add src/tui/event.rs src/tui/cache.rs
git commit -m "feat(tui): add ContextDone/ContextSnippetDone events and context cache"
```

---

## Task 3: CLI and main.rs

**Files:**
- Modify: `src/cli/mod.rs`
- Modify: `src/main.rs`

### cli/mod.rs

**Step 1: Add `Context` variant to `TuiMode`**

Find:
```rust
pub enum TuiMode {
    /// Open in search mode (default).
    #[default]
    Search,
    /// Open in impact analysis mode.
    Impact,
}
```
Replace with:
```rust
pub enum TuiMode {
    /// Open in search mode (default).
    #[default]
    Search,
    /// Open in impact analysis mode.
    Impact,
    /// Open in context mode.
    Context,
}
```

Also update the doc-comment on the `Tui` command's `--mode` arg to mention context:
Find: `"Which mode to open the TUI in: 'search' (default) or 'impact'."`
Replace with: `"Which mode to open the TUI in: 'search' (default), 'impact', or 'context'."`

### main.rs

**Step 1: Handle `TuiMode::Context` in `new_loading`**

Find:
```rust
let initial_mode = match mode {
    TuiMode::Search => ActiveMode::Search,
    TuiMode::Impact => ActiveMode::Impact,
};
```
(There are **two** such blocks in `app.rs` — but in `main.rs` this pattern does NOT appear; the mode is passed directly to `TuiApp::new_loading`. So only `app.rs` needs updating — handled in Task 4. No changes to `main.rs` are needed beyond confirming this.)

Actually, re-read `main.rs:242`: `TuiApp::new_loading(repository, mode, query, tx, rx)` — `mode` is `TuiMode`, the mapping happens inside `TuiApp::new_loading`. So `main.rs` needs no changes.

**Step 2: Build check**

```bash
cargo build 2>&1 | head -50
```

**Step 3: Commit**

```bash
git add src/cli/mod.rs
git commit -m "feat(cli): add Context variant to TuiMode"
```

---

## Task 4: TuiApp wiring (`src/tui/app.rs`)

This is the largest task. Read `src/tui/app.rs` fully before starting.

**Files:**
- Modify: `src/tui/app.rs`

### Step 1: Add imports

Add to the `use` block:
```rust
use crate::application::use_cases::symbol_context::SymbolContextUseCase;
```

### Step 2: Add `context_uc` and `context_task` fields to `TuiApp`

Find the struct definition:
```rust
pub struct TuiApp {
    state: AppState,
    cache: TuiCache,
    search_uc: Option<Arc<SearchCodeUseCase>>,
    impact_uc: Option<Arc<ImpactAnalysisUseCase>>,
    snippet_uc: Option<Arc<SnippetLookupUseCase>>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    impact_task: Option<tokio::task::JoinHandle<()>>,
}
```
Replace with:
```rust
pub struct TuiApp {
    state: AppState,
    cache: TuiCache,
    search_uc: Option<Arc<SearchCodeUseCase>>,
    impact_uc: Option<Arc<ImpactAnalysisUseCase>>,
    snippet_uc: Option<Arc<SnippetLookupUseCase>>,
    context_uc: Option<Arc<SymbolContextUseCase>>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    impact_task: Option<tokio::task::JoinHandle<()>>,
    context_task: Option<tokio::task::JoinHandle<()>>,
}
```

### Step 3: Initialise new fields in `new_loading`

Find:
```rust
Self {
    state: AppState::new(repository, initial_mode, query, false),
    cache: TuiCache::default(),
    search_uc: None,
    impact_uc: None,
    snippet_uc: None,
    event_tx,
    event_rx,
    impact_task: None,
}
```
Replace with:
```rust
Self {
    state: AppState::new(repository, initial_mode, query, false),
    cache: TuiCache::default(),
    search_uc: None,
    impact_uc: None,
    snippet_uc: None,
    context_uc: None,
    event_tx,
    event_rx,
    impact_task: None,
    context_task: None,
}
```

Also update `TuiApp::new` to map `TuiMode::Context` and add the new fields:
```rust
let initial_mode = match mode {
    TuiMode::Search => ActiveMode::Search,
    TuiMode::Impact => ActiveMode::Impact,
    TuiMode::Context => ActiveMode::Context,
};
Self {
    state: AppState::new(repository, initial_mode, query, true),
    cache: TuiCache::default(),
    search_uc: Some(search_uc),
    impact_uc: Some(impact_uc),
    snippet_uc: Some(snippet_uc),
    context_uc: None,  // only set via ContainerReady in this path
    event_tx: tx,
    event_rx: rx,
    impact_task: None,
    context_task: None,
}
```

Wait — `TuiApp::new` takes explicit use-case args (no container). Looking at the current signature, it doesn't accept `context_uc`. For now, leave `context_uc: None` in the `new` path (this path is used in tests; context can be added later). The important path is `new_loading` / `ContainerReady`.

### Step 4: Handle `TuiMode::Context` in `new_loading`

Find in `new_loading`:
```rust
let initial_mode = match mode {
    TuiMode::Search => ActiveMode::Search,
    TuiMode::Impact => ActiveMode::Impact,
};
```
Replace with:
```rust
let initial_mode = match mode {
    TuiMode::Search => ActiveMode::Search,
    TuiMode::Impact => ActiveMode::Impact,
    TuiMode::Context => ActiveMode::Context,
};
```

### Step 5: Set `context_uc` in `ContainerReady` handler

Find inside `TuiEvent::ContainerReady(result) => { Ok(container) => { ... } }`:
```rust
self.search_uc = Some(Arc::new(container.search_use_case()));
self.impact_uc = Some(Arc::new(container.impact_use_case()));
self.snippet_uc = Some(Arc::new(container.snippet_lookup_use_case()));
self.state.models_ready = true;
```
Replace with:
```rust
self.search_uc = Some(Arc::new(container.search_use_case()));
self.impact_uc = Some(Arc::new(container.impact_use_case()));
self.snippet_uc = Some(Arc::new(container.snippet_lookup_use_case()));
self.context_uc = Some(Arc::new(container.context_use_case()));
self.state.models_ready = true;
```

Also add auto-dispatch for Context in the same block, after the existing auto-dispatch:
```rust
// If the user had pre-typed a query (via --query CLI arg),
// auto-dispatch it now that models are ready.
match self.state.mode {
    ActiveMode::Search if !self.state.search.input.is_empty() => {
        self.dispatch_search();
    }
    ActiveMode::Impact if !self.state.impact.input.is_empty() => {
        self.dispatch_impact();
    }
    ActiveMode::Context if !self.state.context.input.is_empty() => {
        self.dispatch_context();
    }
    _ => {}
}
```

Also add context error handling in the `Err(e)` branch inside `ContainerReady`:
```rust
Err(e) => {
    warn!("container init failed: {}", e);
    match self.state.mode {
        ActiveMode::Search => {
            self.state.search.error = Some(format!("Model load error: {e}"));
        }
        ActiveMode::Impact => {
            self.state.impact.error = Some(format!("Model load error: {e}"));
        }
        ActiveMode::Context => {
            self.state.context.error = Some(format!("Model load error: {e}"));
        }
    }
}
```

### Step 6: Handle `ContextDone` and `ContextSnippetDone` in `handle_app_event`

Add two new match arms at the bottom of `handle_app_event` (before the closing `}`):

```rust
TuiEvent::ContextDone { key, result } => {
    if self.state.context.pending_key.as_deref() != Some(&key) {
        return;
    }
    self.state.context.pending_key = None;
    self.state.context.loading = false;
    match result {
        Ok(ctx) => {
            self.cache.contexts.insert(key, ctx.clone());
            self.state.context.context = Some(ctx);
            self.state.context.selected = 0;
            self.state.context.tree_scroll = 0;
        }
        Err(e) => {
            self.state.context.errored_key = Some(key);
            self.state.context.error = Some(e);
        }
    }
}
TuiEvent::ContextSnippetDone { key, result } => {
    if self.state.context.chain_snippet_pending_key.as_ref() != Some(&key) {
        return;
    }
    self.state.context.chain_snippet_pending_key = None;
    self.state.context.chain_snippet_loading = false;
    match result {
        Ok(chunk) => {
            self.cache.snippets.insert(key, chunk.clone());
            self.state.context.chain_snippet = chunk;
            self.state.context.chain_snippet_scroll = 0;
        }
        Err(e) => {
            warn!("context snippet lookup failed: {}", e);
        }
    }
}
```

### Step 7: Add `dispatch_context` method (mirrors `dispatch_impact`)

```rust
fn dispatch_context(&mut self) {
    let uc = match &self.context_uc {
        Some(uc) => Arc::clone(uc),
        None => return, // models not yet ready
    };

    let symbol = self.state.context.input.trim().to_string();
    if symbol.is_empty() {
        return;
    }

    let key = TuiCache::context_key(&symbol, self.state.context.repository.as_deref());

    if let Some(cached) = self.cache.contexts.get(&key).cloned() {
        self.state.context.context = Some(cached);
        self.state.context.selected = 0;
        self.state.context.tree_scroll = 0;
        self.state.context.error = None;
        self.state.context.loading = false;
        self.state.context.pending_key = None;
        return;
    }

    if self.state.context.pending_key.as_deref() == Some(&key) {
        return;
    }

    if self.state.context.errored_key.as_deref() == Some(&key) {
        return;
    }

    self.state.context.loading = true;
    self.state.context.error = None;
    self.state.context.context = None;
    self.state.context.selected = 0;
    self.state.context.tree_scroll = 0;
    self.state.context.chain_selected = 0;
    self.state.context.chain_snippet = None;
    self.state.context.chain_snippet_loading = false;
    self.state.context.chain_snippet_pending_key = None;
    self.state.context.chain_snippet_scroll = 0;
    self.state.context.pending_key = Some(key.clone());
    self.state.context.errored_key = None;

    let tx = self.event_tx.clone();
    let repository = self.state.context.repository.clone();

    if let Some(handle) = self.context_task.take() {
        handle.abort();
    }
    self.context_task = Some(tokio::spawn(async move {
        let result = uc
            .get_context(&symbol, repository.as_deref(), false)
            .await
            .map_err(|e| e.to_string());
        if let Err(e) = tx.send(TuiEvent::ContextDone { key, result }) {
            debug!("ContextDone send failed (app already exited): {}", e);
        }
    }));
}
```

### Step 8: Add `dispatch_context_snippet` method (mirrors `dispatch_chain_snippet`)

This requires extracting coords from the selected context chain node. The path is traced using the same `via_symbol` backward-walk logic as the controller. The selected entry-point is identified by `state.context.selected` into the leaf nodes of `callers_by_depth` (built by `leaf_caller_nodes` in the view — here we compute inline).

```rust
fn dispatch_context_snippet(&mut self) {
    let uc = match &self.snippet_uc {
        Some(uc) => Arc::clone(uc),
        None => return,
    };

    let node_coords = {
        let ctx = match &self.state.context.context {
            Some(c) => c,
            None => return,
        };

        // Compute leaf nodes (same logic as views/context.rs).
        let all_callers: Vec<&crate::application::use_cases::symbol_context::ContextNode> =
            ctx.callers_by_depth.iter().flatten().collect();
        let caller_symbols: std::collections::HashSet<&str> =
            all_callers.iter().map(|n| n.symbol.as_str()).collect();
        let mut leaf_nodes: Vec<&crate::application::use_cases::symbol_context::ContextNode> =
            all_callers
                .iter()
                .copied()
                .filter(|n| {
                    !all_callers
                        .iter()
                        .any(|m| m.via_symbol.as_deref() == Some(n.symbol.as_str()))
                })
                .collect();

        // Fallback: if no callers at all, there's nothing to select
        if leaf_nodes.is_empty() {
            return;
        }

        let leaf = match leaf_nodes.get(self.state.context.selected) {
            Some(l) => *l,
            None => leaf_nodes[0],
        };

        // Trace the path from leaf back toward the root symbol.
        let node_by_depth_sym: std::collections::HashMap<(usize, &str), &crate::application::use_cases::symbol_context::ContextNode> =
            all_callers
                .iter()
                .map(|n| ((n.depth, n.symbol.as_str()), *n))
                .collect();

        let mut path = vec![leaf];
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

        let node = match path.get(self.state.context.chain_selected) {
            Some(n) => *n,
            None => return,
        };

        (
            node.repository_id.clone(),
            node.file_path.clone(),
            node.line,
        )
    };

    let key = TuiCache::snippet_key(&node_coords.0, &node_coords.1, node_coords.2);

    if let Some(cached) = self.cache.snippets.get(&key).cloned() {
        self.state.context.chain_snippet = cached;
        self.state.context.chain_snippet_loading = false;
        self.state.context.chain_snippet_pending_key = None;
        self.state.context.chain_snippet_scroll = 0;
        return;
    }

    self.state.context.chain_snippet_loading = true;
    self.state.context.chain_snippet_pending_key = Some(key.clone());
    self.state.context.chain_snippet_scroll = 0;

    let tx = self.event_tx.clone();
    let (repo_id, file_path, line) = node_coords;

    tokio::spawn(async move {
        let result = uc
            .get_snippet(&repo_id, &file_path, line)
            .await
            .map_err(|e| e.to_string());
        if let Err(e) = tx.send(TuiEvent::ContextSnippetDone { key, result }) {
            debug!("ContextSnippetDone send failed (app already exited): {}", e);
        }
    });
}
```

### Step 9: Update `dispatch_current` to handle `ActiveMode::Context`

Find:
```rust
fn dispatch_current(&mut self) {
    if !self.state.models_ready {
        return;
    }
    match self.state.mode {
        ActiveMode::Search => self.dispatch_search(),
        ActiveMode::Impact => {
            match self.state.impact.focused_pane {
                ImpactPane::EntryPoints => self.dispatch_impact(),
                ImpactPane::Chain => {
                    if self.state.impact.chain_snippet.is_none()
                        && !self.state.impact.chain_snippet_loading
                    {
                        self.dispatch_chain_snippet();
                    }
                }
            }
        }
    }
}
```
Replace with:
```rust
fn dispatch_current(&mut self) {
    if !self.state.models_ready {
        return;
    }
    match self.state.mode {
        ActiveMode::Search => self.dispatch_search(),
        ActiveMode::Impact => {
            match self.state.impact.focused_pane {
                ImpactPane::EntryPoints => self.dispatch_impact(),
                ImpactPane::Chain => {
                    if self.state.impact.chain_snippet.is_none()
                        && !self.state.impact.chain_snippet_loading
                    {
                        self.dispatch_chain_snippet();
                    }
                }
            }
        }
        ActiveMode::Context => {
            match self.state.context.focused_pane {
                ContextPane::EntryPoints => self.dispatch_context(),
                ContextPane::Tree => {
                    if self.state.context.chain_snippet.is_none()
                        && !self.state.context.chain_snippet_loading
                    {
                        self.dispatch_context_snippet();
                    }
                }
            }
        }
    }
}
```

You also need to add `ContextPane` to the imports at the top of `app.rs`:
```rust
use super::state::{ActiveMode, AppState, ContextPane, ImpactPane, SearchPane};
```

### Step 10: Update `handle_key` Tab cycling

Find:
```rust
KeyCode::Tab => {
    self.state.mode = match self.state.mode {
        ActiveMode::Search => ActiveMode::Impact,
        ActiveMode::Impact => ActiveMode::Search,
    };
}
```
Replace with:
```rust
KeyCode::Tab => {
    self.state.mode = match self.state.mode {
        ActiveMode::Search => ActiveMode::Impact,
        ActiveMode::Impact => ActiveMode::Context,
        ActiveMode::Context => ActiveMode::Search,
    };
}
```

### Step 11: Add `Ctrl+↓` → jump to Context

After the `Ctrl+↑` handler:
```rust
KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
    self.jump_to_impact();
}
```
Add:
```rust
KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
    self.jump_to_context();
}
```

### Step 12: Add `jump_to_context` method (mirrors `jump_to_impact`)

```rust
fn jump_to_context(&mut self) {
    if self.state.mode != ActiveMode::Search {
        return;
    }
    let symbol = self
        .state
        .search
        .results
        .get(self.state.search.selected)
        .and_then(|r| {
            r.chunk()
                .qualified_name()
                .or_else(|| r.chunk().symbol_name().map(str::to_string))
        });

    if let Some(sym) = symbol {
        self.state.context.cursor = sym.chars().count();
        self.state.context.input = sym;
        self.state.context.context = None;
        self.state.context.selected = 0;
        self.state.context.tree_scroll = 0;
        self.state.context.chain_selected = 0;
        self.state.context.chain_snippet = None;
        self.state.context.chain_snippet_loading = false;
        self.state.context.chain_snippet_pending_key = None;
        self.state.context.chain_snippet_scroll = 0;
        self.state.context.focused_pane = ContextPane::EntryPoints;
        self.state.mode = ActiveMode::Context;
        self.dispatch_context();
    }
}
```

### Step 13: Update the char-typing guard for Context mode

Find the guard that resets focus when typing in Impact's Chain pane:
```rust
if self.state.mode == ActiveMode::Impact
    && self.state.impact.focused_pane == ImpactPane::Chain
{
    self.state.impact.focused_pane = ImpactPane::EntryPoints;
    self.state.impact.chain_snippet = None;
    self.state.impact.chain_snippet_loading = false;
    self.state.impact.chain_snippet_pending_key = None;
    self.state.impact.chain_snippet_scroll = 0;
}
```
Add an analogous guard for Context right after it:
```rust
if self.state.mode == ActiveMode::Context
    && self.state.context.focused_pane == ContextPane::Tree
{
    self.state.context.focused_pane = ContextPane::EntryPoints;
    self.state.context.chain_snippet = None;
    self.state.context.chain_snippet_loading = false;
    self.state.context.chain_snippet_pending_key = None;
    self.state.context.chain_snippet_scroll = 0;
}
```

### Step 14: Update `focus_left` and `focus_right` for Context

Find `fn focus_left`:
```rust
fn focus_left(&mut self) {
    match self.state.mode {
        ActiveMode::Search => { self.state.search.focused_pane = SearchPane::List; }
        ActiveMode::Impact => { self.state.impact.focused_pane = ImpactPane::EntryPoints; }
    }
}
```
Replace with:
```rust
fn focus_left(&mut self) {
    match self.state.mode {
        ActiveMode::Search => { self.state.search.focused_pane = SearchPane::List; }
        ActiveMode::Impact => { self.state.impact.focused_pane = ImpactPane::EntryPoints; }
        ActiveMode::Context => { self.state.context.focused_pane = ContextPane::EntryPoints; }
    }
}
```

Find `fn focus_right`:
```rust
fn focus_right(&mut self) {
    match self.state.mode {
        ActiveMode::Search => { self.state.search.focused_pane = SearchPane::Code; }
        ActiveMode::Impact => {
            self.state.impact.focused_pane = ImpactPane::Chain;
            self.state.impact.chain_selected = 0;
        }
    }
}
```
Replace with:
```rust
fn focus_right(&mut self) {
    match self.state.mode {
        ActiveMode::Search => { self.state.search.focused_pane = SearchPane::Code; }
        ActiveMode::Impact => {
            self.state.impact.focused_pane = ImpactPane::Chain;
            self.state.impact.chain_selected = 0;
        }
        ActiveMode::Context => {
            self.state.context.focused_pane = ContextPane::Tree;
            self.state.context.chain_selected = 0;
        }
    }
}
```

### Step 15: Update `handle_esc` for Context

Find:
```rust
fn handle_esc(&mut self) {
    if self.state.mode == ActiveMode::Impact && self.state.impact.chain_snippet.is_some() {
        self.state.impact.chain_snippet = None;
        self.state.impact.chain_snippet_loading = false;
        self.state.impact.chain_snippet_pending_key = None;
        self.state.impact.chain_snippet_scroll = 0;
    }
}
```
Replace with:
```rust
fn handle_esc(&mut self) {
    if self.state.mode == ActiveMode::Impact && self.state.impact.chain_snippet.is_some() {
        self.state.impact.chain_snippet = None;
        self.state.impact.chain_snippet_loading = false;
        self.state.impact.chain_snippet_pending_key = None;
        self.state.impact.chain_snippet_scroll = 0;
    }
    if self.state.mode == ActiveMode::Context && self.state.context.chain_snippet.is_some() {
        self.state.context.chain_snippet = None;
        self.state.context.chain_snippet_loading = false;
        self.state.context.chain_snippet_pending_key = None;
        self.state.context.chain_snippet_scroll = 0;
    }
}
```

### Step 16: Update `navigate` for Context

Add a `ActiveMode::Context` arm to `fn navigate`:

```rust
ActiveMode::Context => {
    if self.state.context.focused_pane == ContextPane::Tree {
        if self.state.context.chain_snippet.is_some() {
            self.state.context.chain_snippet_scroll = bounded_scroll(
                self.state.context.chain_snippet_scroll,
                delta * SCROLL_STEP as i32,
            );
        } else {
            self.navigate_context_chain(delta);
        }
        return;
    }
    // Left pane (entry points) — need to know how many leaf nodes we have.
    // We cannot call into views from here, so re-compute inline.
    let len = self.state.context.context.as_ref().map(|ctx| {
        let all_callers: Vec<_> = ctx.callers_by_depth.iter().flatten().collect();
        all_callers
            .iter()
            .filter(|n| {
                !all_callers
                    .iter()
                    .any(|m| m.via_symbol.as_deref() == Some(n.symbol.as_str()))
            })
            .count()
        .max(if ctx.total_callers == 0 { 1 } else { 0 })
    }).unwrap_or(0);
    if len == 0 {
        return;
    }
    let old = self.state.context.selected;
    self.state.context.selected = bounded_add(old, delta, len);
    if self.state.context.selected != old {
        self.state.context.chain_selected = 0;
        self.state.context.chain_snippet = None;
        self.state.context.chain_snippet_loading = false;
        self.state.context.chain_snippet_pending_key = None;
        self.state.context.chain_snippet_scroll = 0;
    }
}
```

Add helper `navigate_context_chain`:

```rust
fn navigate_context_chain(&mut self, delta: i32) {
    let len = self.state.context.context.as_ref().and_then(|ctx| {
        let all_callers: Vec<_> = ctx.callers_by_depth.iter().flatten().collect();
        let mut leaf_nodes: Vec<_> = all_callers
            .iter()
            .copied()
            .filter(|n| {
                !all_callers
                    .iter()
                    .any(|m| m.via_symbol.as_deref() == Some(n.symbol.as_str()))
            })
            .collect();
        if leaf_nodes.is_empty() {
            return Some(0);
        }
        let leaf = leaf_nodes.get(self.state.context.selected).copied()?;
        // Trace path length.
        let node_by_depth_sym: std::collections::HashMap<(usize, &str), _> =
            all_callers.iter().map(|n| ((n.depth, n.symbol.as_str()), *n)).collect();
        let mut path_len = 1usize;
        let mut current = leaf;
        while let Some(via) = current.via_symbol.as_deref() {
            let parent_depth = current.depth.saturating_sub(1);
            if let Some(&parent) = node_by_depth_sym.get(&(parent_depth, via)) {
                path_len += 1;
                current = parent;
            } else {
                break;
            }
        }
        Some(path_len)
    }).unwrap_or(0);

    if len == 0 {
        return;
    }
    self.state.context.chain_selected =
        bounded_add(self.state.context.chain_selected, delta, len);
}
```

### Step 17: Update `scroll_code` for Context

Add a `ActiveMode::Context` arm:
```rust
ActiveMode::Context => {
    if self.state.context.chain_snippet.is_some() {
        self.state.context.chain_snippet_scroll =
            bounded_scroll(self.state.context.chain_snippet_scroll, delta);
    } else {
        self.state.context.tree_scroll =
            bounded_scroll(self.state.context.tree_scroll, delta);
    }
}
```

### Step 18: Update `run_with_terminal` for Context auto-dispatch

Find the match in `run_with_terminal`:
```rust
match self.state.mode {
    ActiveMode::Search if !self.state.search.input.is_empty() => {
        self.dispatch_search();
    }
    ActiveMode::Impact if !self.state.impact.input.is_empty() => {
        self.dispatch_impact();
    }
    _ => {}
}
```
Replace with:
```rust
match self.state.mode {
    ActiveMode::Search if !self.state.search.input.is_empty() => {
        self.dispatch_search();
    }
    ActiveMode::Impact if !self.state.impact.input.is_empty() => {
        self.dispatch_impact();
    }
    ActiveMode::Context if !self.state.context.input.is_empty() => {
        self.dispatch_context();
    }
    _ => {}
}
```

### Step 19: Build check

```bash
cargo build 2>&1 | head -80
```

Fix any compile errors. Common issues:
- Missing import for `SymbolContextUseCase` — add it.
- `ContextNode` used inline — it's from `crate::application::use_cases::symbol_context::ContextNode`, add a `use` at the top of the method or at the top of the file.
- Non-exhaustive match arms elsewhere — those are handled in later tasks (views).

### Step 20: Commit

```bash
git add src/tui/app.rs
git commit -m "feat(tui): wire SymbolContextUseCase dispatch, events and key handling into TuiApp"
```

---

## Task 5: Input bar (third tab)

**Files:**
- Modify: `src/tui/widgets/input_bar.rs`

### Step 1: Update `render` to show three tabs

Find:
```rust
let (search_style, impact_style) = tab_styles(&state.mode);

let title = Line::from(vec![
    Span::styled(" Search ", search_style),
    Span::raw("  "),
    Span::styled(" Impact ", impact_style),
    Span::raw(" "),
]);
```
Replace with:
```rust
let (search_style, impact_style, context_style) = tab_styles(&state.mode);

let title = Line::from(vec![
    Span::styled(" Search ", search_style),
    Span::raw("  "),
    Span::styled(" Impact ", impact_style),
    Span::raw("  "),
    Span::styled(" Context ", context_style),
    Span::raw(" "),
]);
```

### Step 2: Update `tab_styles` to return three styles

Find:
```rust
fn tab_styles(mode: &ActiveMode) -> (Style, Style) {
    let active = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(Color::DarkGray);

    match mode {
        ActiveMode::Search => (active, inactive),
        ActiveMode::Impact => (inactive, active),
    }
}
```
Replace with:
```rust
fn tab_styles(mode: &ActiveMode) -> (Style, Style, Style) {
    let active = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(Color::DarkGray);

    match mode {
        ActiveMode::Search => (active, inactive.clone(), inactive),
        ActiveMode::Impact => (inactive.clone(), active, inactive),
        ActiveMode::Context => (inactive.clone(), inactive, active),
    }
}
```

### Step 3: Build check

```bash
cargo build 2>&1 | head -40
```

### Step 4: Commit

```bash
git add src/tui/widgets/input_bar.rs
git commit -m "feat(tui): add Context tab to input bar"
```

---

## Task 6: Context view (`src/tui/views/context.rs`)

This is the rendering module. Study `src/tui/views/impact.rs` in full before implementing.

**Files:**
- Create: `src/tui/views/context.rs`

### Step 1: Create the file with module-level helpers

```rust
use std::collections::{HashMap, HashSet};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::application::use_cases::symbol_context::{ContextNode, SymbolContext};
use crate::tui::state::{AppState, ContextPane};
use crate::tui::widgets::result_list;
use crate::tui::widgets::result_list::ListEntry;
use crate::tui::widgets::syntax;

use super::format::{short_symbol, shorten_path};

/// Public entry point called by `views/mod.rs`.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);

    render_entry_points(frame, panes[0], state);
    render_right(frame, panes[1], state);
}
```

### Step 2: Add `leaf_caller_nodes` helper

This extracts leaf nodes (entry-points) from the callers BFS. A leaf is a node whose own symbol is not used as the `via_symbol` of any other node (i.e., no other node is a caller "through" this node).

```rust
/// Extract the top-most entry-point nodes from the callers BFS.
///
/// A node is a leaf (entry-point) if no other caller node lists it as its
/// `via_symbol`. Mirrors the same logic in `SymbolContextController::format_text`.
pub fn leaf_caller_nodes(ctx: &SymbolContext) -> Vec<&ContextNode> {
    let all_callers: Vec<&ContextNode> = ctx.callers_by_depth.iter().flatten().collect();
    all_callers
        .iter()
        .copied()
        .filter(|n| {
            !all_callers
                .iter()
                .any(|m| m.via_symbol.as_deref() == Some(n.symbol.as_str()))
        })
        .collect()
}

/// Build the caller path from a leaf node back to the direct caller of the root symbol.
///
/// Returns a Vec where index 0 is the leaf (top-most caller), last index is the
/// direct caller of the root symbol.
pub fn trace_caller_path<'a>(
    leaf: &'a ContextNode,
    all_callers: &[&'a ContextNode],
) -> Vec<&'a ContextNode> {
    let node_by_depth_sym: HashMap<(usize, &str), &ContextNode> = all_callers
        .iter()
        .map(|n| ((n.depth, n.symbol.as_str()), *n))
        .collect();

    let mut path = vec![leaf];
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
    path
}

/// Build a map from parent_symbol → direct callee ContextNodes.
fn build_callee_children_map<'a>(ctx: &'a SymbolContext) -> HashMap<String, Vec<&'a ContextNode>> {
    let mut map: HashMap<String, Vec<&'a ContextNode>> = HashMap::new();
    for node in ctx.callees_by_depth.iter().flatten() {
        let key = node.via_symbol.as_deref().unwrap_or(&ctx.symbol).to_owned();
        map.entry(key).or_default().push(node);
    }
    // Synthetic entry for multi-root display label (mirrors controller logic).
    let root_sym_set: HashSet<&str> = ctx.root_symbols.iter().map(String::as_str).collect();
    let depth1_nodes: Vec<&ContextNode> = ctx
        .callees_by_depth
        .first()
        .map(|d| d.iter().collect())
        .unwrap_or_default();
    if !root_sym_set.contains(ctx.symbol.as_str()) && !depth1_nodes.is_empty() {
        map.insert(ctx.symbol.clone(), depth1_nodes);
    }
    map
}
```

### Step 3: Add `render_entry_points`

```rust
fn render_entry_points(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.context;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Entry-points ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        frame.render_widget(
            Paragraph::new(format!("Error: {}", err))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let entries: Vec<ListEntry> = match &s.context {
        None => vec![],
        Some(ctx) => {
            let leaves = leaf_caller_nodes(ctx);
            if leaves.is_empty() {
                // No callers: show a synthetic "callees only" entry.
                vec![ListEntry {
                    label: format!("◉  {}", short_symbol(&ctx.symbol)),
                    sub_label: Some("no callers — callees only".to_string()),
                    score: None,
                }]
            } else {
                leaves
                    .iter()
                    .map(|node| ListEntry {
                        label: format!("{}:{}", shorten_path(&node.file_path), node.line),
                        sub_label: Some(short_symbol(&node.symbol).to_string()),
                        score: None,
                    })
                    .collect()
            }
        }
    };

    let title = format!("Entry-points ({})", entries.len());
    result_list::render(frame, area, &title, &entries, s.selected);
}
```

### Step 4: Add `render_right`

```rust
fn render_right(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.context;
    let tree_focused = s.focused_pane == ContextPane::Tree;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Call context ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        frame.render_widget(
            Paragraph::new(format!("Error: {}", err))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let ctx = match &s.context {
        None => {
            let block = Block::default()
                .title(" Call context ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            let hint = if s.loading {
                "  Analysing…"
            } else {
                "  Enter a symbol and press Enter."
            };
            frame.render_widget(
                Paragraph::new(hint)
                    .block(block)
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }
        Some(c) => c,
    };

    // Code view: if a chain node snippet is loaded or loading, show it.
    if s.chain_snippet_loading {
        let border_color = if tree_focused { Color::Cyan } else { Color::White };
        let block = Block::default()
            .title(" Code ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        frame.render_widget(
            Paragraph::new("  Loading…")
                .block(block)
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    if let Some(chunk) = &s.chain_snippet {
        let border_color = if tree_focused { Color::Cyan } else { Color::White };
        let title = format!(" {} ", shorten_path(chunk.file_path()));
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = syntax::highlight_code(
            chunk.content(),
            chunk.file_path(),
            chunk.start_line() as usize,
        );

        let para = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((s.chain_snippet_scroll, 0));
        frame.render_widget(para, inner);
        return;
    }

    // Default: call context tree.
    let border_color = if tree_focused { Color::Cyan } else { Color::White };

    let all_callers: Vec<&ContextNode> = ctx.callers_by_depth.iter().flatten().collect();
    let leaves = leaf_caller_nodes(ctx);

    let (path, has_callers) = if leaves.is_empty() {
        // No callers: just show the callees subtree from the root symbol.
        (vec![], false)
    } else {
        let leaf = leaves.get(s.selected).copied().unwrap_or(leaves[0]);
        let path = trace_caller_path(leaf, &all_callers);
        (path, true)
    };

    let callee_children = build_callee_children_map(ctx);

    render_call_context_tree(
        frame,
        area,
        &path,
        &ctx.symbol,
        &callee_children,
        has_callers,
        tree_focused,
        s.chain_selected,
        s.tree_scroll,
        border_color,
    );
}
```

### Step 5: Add `render_call_context_tree`

This is the core rendering function. It extends the Impact `render_path_tree` by adding the callees subtree below the `◉` root symbol.

```rust
/// Render the full call context tree:
///
/// ```text
/// ★  entry_point  file:line         ← selectable (index 0)
///    │
///    └── intermediate  file:line   ← selectable (index 1)
///        │
///        └── ◉  root_symbol        ← NOT selectable (terminal of caller chain)
///            ├── child_A  file:line ← NOT selectable (callee)
///            └── child_B  file:line ← NOT selectable (callee)
///                └── grandchild
/// ```
#[allow(clippy::too_many_arguments)]
fn render_call_context_tree(
    frame: &mut Frame,
    area: Rect,
    path: &[&ContextNode],      // caller chain, leaf-first; empty when no callers
    root_symbol: &str,
    callee_children: &HashMap<String, Vec<&ContextNode>>,
    has_callers: bool,
    tree_focused: bool,
    selected: usize,
    scroll: u16,
    border_color: Color,
) {
    let block = Block::default()
        .title(" Call context ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    if has_callers {
        // ── Caller chain (same style as impact's render_path_tree) ────────────

        // First row: entry point (path[0]).
        if let Some(leaf) = path.first() {
            let is_sel = tree_focused && selected == 0;
            let fg = if is_sel { Color::Black } else { Color::Cyan };
            let bg = if is_sel { Color::Cyan } else { Color::Reset };
            let marker = if is_sel { "▶ ★  " } else { "  ★  " };

            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan).bg(bg)),
                Span::styled(
                    short_symbol(&leaf.symbol).to_string(),
                    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}:{}", shorten_path(&leaf.file_path), leaf.line),
                    Style::default()
                        .fg(if is_sel { Color::Black } else { Color::DarkGray })
                        .bg(bg),
                ),
            ]));
        }

        // Intermediate nodes (path[1..]).
        for (idx, node) in path.iter().skip(1).enumerate() {
            let node_idx = idx + 1;
            let is_sel = tree_focused && selected == node_idx;
            let base_indent = "    ".repeat(idx);

            lines.push(Line::from(Span::styled(
                format!("{}   │", base_indent),
                Style::default().fg(Color::DarkGray),
            )));

            let fg = if is_sel { Color::Black } else { Color::White };
            let bg = if is_sel { Color::White } else { Color::Reset };
            let marker = if is_sel { "▶ " } else { "  " };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}   └── {}", base_indent, marker),
                    Style::default().fg(Color::DarkGray).bg(bg),
                ),
                Span::styled(
                    short_symbol(&node.symbol).to_string(),
                    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}:{}", shorten_path(&node.file_path), node.line),
                    Style::default()
                        .fg(if is_sel { Color::Black } else { Color::DarkGray })
                        .bg(bg),
                ),
            ]));
        }

        // Root symbol (◉) — not selectable, terminating the caller chain.
        {
            let depth = path.len().saturating_sub(1);
            let base_indent = "    ".repeat(depth);
            lines.push(Line::from(Span::styled(
                format!("{}   │", base_indent),
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}   └── ", base_indent),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled("◉  ", Style::default().fg(Color::Red)),
                Span::styled(
                    root_symbol.to_string(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
            ]));

            // Callee subtree hangs off the root symbol.
            let callee_prefix = "    ".repeat(depth + 1);
            let mut visited: HashSet<String> = HashSet::new();
            render_callees_subtree(
                root_symbol,
                callee_children,
                &callee_prefix,
                &mut lines,
                &mut visited,
            );
        }
    } else {
        // No callers: just root symbol + callees.
        lines.push(Line::from(vec![
            Span::styled("◉  ", Style::default().fg(Color::Red)),
            Span::styled(
                root_symbol.to_string(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]));
        let mut visited: HashSet<String> = HashSet::new();
        render_callees_subtree(root_symbol, callee_children, "    ", &mut lines, &mut visited);
    }

    let visible: Vec<Line> = lines
        .into_iter()
        .skip(scroll as usize)
        .take(inner.height as usize)
        .collect();

    frame.render_widget(Paragraph::new(visible), inner);
}

/// Recursively render the callees subtree rooted at `parent_symbol`.
/// Mirrors `SymbolContextController::render_callees_subtree` but outputs `Line` instead of text.
fn render_callees_subtree(
    parent_symbol: &str,
    callee_children: &HashMap<String, Vec<&ContextNode>>,
    prefix: &str,
    lines: &mut Vec<Line>,
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
        let is_last = i == count - 1;
        let branch = if is_last { "└──" } else { "├──" };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{}{} ", prefix, branch),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                short_symbol(&node.symbol).to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}:{}", shorten_path(&node.file_path), node.line),
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        let child_prefix = if is_last {
            format!("{}    ", prefix)
        } else {
            format!("{}│   ", prefix)
        };
        render_callees_subtree(&node.symbol, callee_children, &child_prefix, lines, visited);
    }
}
```

### Step 6: Build check

```bash
cargo build 2>&1 | head -60
```

### Step 7: Commit

```bash
git add src/tui/views/context.rs
git commit -m "feat(tui): add context view with entry-point list and call-context tree renderer"
```

---

## Task 7: Wire view into `views/mod.rs` and update status bar

**Files:**
- Modify: `src/tui/views/mod.rs`

### Step 1: Declare the new module

Add at top of the file:
```rust
mod context;
```

### Step 2: Update the mode routing match

Find:
```rust
match state.mode {
    ActiveMode::Search => search::render(frame, areas[1], state),
    ActiveMode::Impact => impact::render(frame, areas[1], state),
}
```
Replace with:
```rust
match state.mode {
    ActiveMode::Search => search::render(frame, areas[1], state),
    ActiveMode::Impact => impact::render(frame, areas[1], state),
    ActiveMode::Context => context::render(frame, areas[1], state),
}
```

### Step 3: Update imports

Add `ContextPane` to the existing import from state:
```rust
use crate::tui::state::{ActiveMode, AppState, ContextPane, ImpactPane, SearchPane};
```

### Step 4: Add `ActiveMode::Context` arm to `render_status`

Add after the `ActiveMode::Impact` arm:
```rust
ActiveMode::Context => match state.context.focused_pane {
    ContextPane::EntryPoints => {
        " Enter: analyse  ↑↓: navigate  ←→: cursor  Ctrl+→: focus tree  Tab: switch  q: quit"
    }
    ContextPane::Tree => {
        if state.context.chain_snippet.is_some() {
            " ↑↓/PgUp/Dn: scroll  Esc: back to tree  q: quit"
        } else {
            " ↑↓: select node  Enter: view code  Ctrl+←: focus list  Tab: switch  q: quit"
        }
    }
},
```

### Step 5: Build check

```bash
cargo build 2>&1 | head -40
```

### Step 6: Run tests

```bash
cargo test 2>&1 | tail -30
```
Expected: all tests pass. If any fail, fix them.

### Step 7: Commit

```bash
git add src/tui/views/mod.rs
git commit -m "feat(tui): route ActiveMode::Context to context view and add status bar hints"
```

---

## Task 8: Update the status-bar hint for Search mode (Ctrl+↓ shortcut)

**Files:**
- Modify: `src/tui/views/mod.rs`

### Step 1: Add the Ctrl+↓ hint to the Search → List status bar

Find:
```rust
SearchPane::List => {
    " Enter: search  ↑↓: navigate  ←→: cursor  Ctrl+→: focus code  Ctrl+↑: impact  Tab: switch  q: quit"
}
```
Replace with:
```rust
SearchPane::List => {
    " Enter: search  ↑↓: navigate  ←→: cursor  Ctrl+→: code  Ctrl+↑: impact  Ctrl+↓: context  Tab: switch  q: quit"
}
```

### Step 2: Build and test

```bash
cargo build && cargo test 2>&1 | tail -20
```

### Step 3: Commit

```bash
git add src/tui/views/mod.rs
git commit -m "feat(tui): add Ctrl+Down context shortcut hint to Search status bar"
```

---

## Task 9: Final integration check

### Step 1: Full build + test pass

```bash
cargo build --release 2>&1 | tail -20
cargo test 2>&1 | tail -30
```

Expected: zero errors, all tests pass.

### Step 2: Clippy check

```bash
cargo clippy 2>&1 | grep "^error" | head -20
```

Fix any `error`-level lints. Warnings are acceptable (fix them if straightforward).

### Step 3: Format check

```bash
cargo fmt --check 2>&1
```

If there are differences:
```bash
cargo fmt
git add -u
git commit -m "chore: apply fmt after TUI context mode implementation"
```

### Step 4: Final commit if needed

If the `cargo fmt` step added a commit, you are done. Otherwise:
```bash
git log --oneline -10
```
Verify all feature commits are present.

---

## Summary of commits expected

After all tasks, `git log --oneline` should show (newest first):

```
chore: apply fmt after TUI context mode implementation  (if needed)
feat(tui): add Ctrl+Down context shortcut hint to Search status bar
feat(tui): route ActiveMode::Context to context view and add status bar hints
feat(tui): add context view with entry-point list and call-context tree renderer
feat(tui): add Context tab to input bar
feat(tui): wire SymbolContextUseCase dispatch, events and key handling into TuiApp
feat(cli): add Context variant to TuiMode
feat(tui): add ContextDone/ContextSnippetDone events and context cache
feat(tui): add ContextState, ContextPane, ActiveMode::Context to AppState
docs: add TUI context mode design doc
```
