# Editor Integrations

CodeSearch provides output formats and plugins for integrating semantic search into your editor workflow.

## Output Formats

The `--format` (`-F`) flag controls output for `search`, `context`, `impact`,
`explain`, `features`, and `clusters`:

| Format | Description | Commands |
|--------|-------------|----------|
| `text` | Human-readable output with code previews (default) | all |
| `json` | Structured JSON array for programmatic consumption | all |
| `vimgrep` | `file:line:col:text` for Neovim quickfix list and Telescope | `search`, `context`, `impact`, `features` |

> `clusters list` / `clusters get` support `text` and `json` only; `vimgrep` is not
> available for them. `clusters overview` always emits a Markdown table.

## Zed

### MCP Context Server (AI assistant integration)

Add the following block to `~/.config/zed/settings.json` (see [`ide/zed/settings.json`](../../ide/zed/settings.json) for a copy-pasteable snippet):

```json
{
  "context_servers": {
    "codesearch": {
      "command": {
        "path": "codesearch",
        "args": ["mcp"]
      },
      "settings": {}
    }
  }
}
```

Restart Zed and open the AI assistant — the server will be listed in the context-server panel. The assistant can then call `search_code`, `analyze_impact`, `get_symbol_context`, and `query_graph` autonomously while you chat.

#### Exposed MCP tools

| Tool | Description |
|------|-------------|
| `search_code` | Hybrid/semantic search (`query`, `limit`, `min_score`, `languages`, `repositories`, `text_search`) |
| `analyze_impact` | Blast-radius analysis for a symbol (`symbol`, `repository_id`, `regex`) |
| `get_symbol_context` | 360° caller/callee context for a symbol (`symbol`, `repository_id`, `regex`) |
| `query_graph` | Single-relationship graph query (`pattern`, `target`, `repository_id`, `limit`) |

`query_graph` accepts one of eight intention-named `pattern`s: `callers_of`,
`callees_of`, `imports_of`, `importers_of`, `inheritors_of`, `children_of`,
`tests_for`, and `file_summary`. See
[Architecture & Dependency Analysis](./architecture-analysis.md#querying-the-graph-from-ai-tools-query_graph)
for what each pattern returns.

### Tasks (command palette integration)

Tasks let you run searches from Zed's command palette (`cmd-shift-p` → "task: spawn") and display results in the terminal panel. They use Zed's built-in task variables:

| Variable | Value |
|---|---|
| `$ZED_SELECTED_TEXT` | Currently selected text |
| `$ZED_SYMBOL` | Symbol under the cursor (from the language server) |
| `$ZED_WORKTREE_ROOT` | Absolute path of the project root |

Run [`ide/zed/setup.sh`](../../ide/zed/setup.sh) for an automated install, or manually copy [`ide/zed/tasks.json`](../../ide/zed/tasks.json) to your project's `.zed/tasks.json` (or merge it into `~/.config/zed/tasks.json` for a global install).

#### Keybindings

Copy [`ide/zed/keybindings.json`](../../ide/zed/keybindings.json) into `~/.config/zed/keymap.json` to add keyboard shortcuts, or let `setup.sh` handle it interactively.

| Keybinding | Action | Context |
|---|---|---|
| `ctrl-shift-f` | codesearch: tui | Global |
| `ctrl-shift-i` | codesearch: tui impact | Editor |

### TUI (interactive terminal UI)

All codesearch tasks open the built-in interactive TUI — a full-screen terminal interface with search, impact analysis, and symbol context in a single pane. No external tools required.

#### How each task works

| Task | Command | What it does |
|---|---|---|
| `codesearch: tui` | `codesearch tui` | Opens the TUI in search mode |
| `codesearch: tui impact` | `codesearch tui --mode impact --query "$ZED_SYMBOL"` | Opens the TUI pre-loaded with the symbol under the cursor for instant impact analysis |
| `codesearch: index current directory` | `codesearch index $ZED_WORKTREE_ROOT` | Indexes (or re-indexes) the current project |

All tasks are defined in [`ide/zed/tasks.json`](../../ide/zed/tasks.json) and installed by `setup.sh`.

#### TUI options passed from Zed

| Flag | Purpose |
|---|---|
| `--mode impact` | Start in impact analysis mode instead of search mode |
| `--query <text>` | Pre-populate the input and immediately dispatch the query |

## Neovim

### Telescope Extension

A [Telescope](https://github.com/nvim-telescope/telescope.nvim) extension is included under [`ide/nvim/`](../../ide/nvim/). It provides a fuzzy picker over semantic search results, with file preview scrolled to the correct line.

#### Prerequisites

- Neovim 0.9+
- [telescope.nvim](https://github.com/nvim-telescope/telescope.nvim)
- `codesearch` binary on your `$PATH`

#### Setup

1. Add the plugin's Lua directory to your Neovim runtime path:

```lua
vim.opt.runtimepath:append("/path/to/codesearch/ide/nvim")
```

2. Load the extension:

```lua
require("telescope").load_extension("codesearch")
```

3. Bind a key:

```lua
vim.keymap.set("n", "<leader>cs", function()
  require("telescope").extensions.codesearch.codesearch()
end, { desc = "Semantic code search" })
```

#### Configuration

Pass options via `telescope.setup()`:

```lua
require("telescope").setup({
  extensions = {
    codesearch = {
      bin = "codesearch",     -- path to the codesearch binary
      num = 20,               -- number of results to fetch
      min_score = nil,        -- minimum relevance score (0.0-1.0)
      language = nil,         -- filter by language (string or list)
      repository = nil,       -- filter by repository (string or list)
      data_dir = nil,         -- custom --data-dir
      namespace = nil,        -- custom --namespace
    },
  },
})
```

#### Usage

```vim
" Open the picker with a prompt
:Telescope codesearch

" Or pass a query directly
:lua require("telescope").extensions.codesearch.codesearch({ query = "error handling" })
```

Inside the picker:
- Type to filter results further
- `<CR>` opens the file at the matched line
- Preview pane shows the file scrolled to the result

#### Per-call Overrides

You can override any option per invocation:

```lua
require("telescope").extensions.codesearch.codesearch({
  query = "authentication",
  language = "rust",
  num = 30,
})
```

### Quickfix List (no plugin required)

You can use the `vimgrep` output format to populate Neovim's quickfix list directly:

```bash
codesearch search "error handling" --format vimgrep | nvim -q /dev/stdin
```

Or from within Neovim:

```vim
:cexpr system('codesearch search "error handling" --format vimgrep')
:copen
```

### JSON Format

The `json` format is useful for building custom integrations or piping into other tools:

```bash
codesearch search "authentication" --format json | jq '.[].file_path'
```

Each result object contains:

| Field | Type | Description |
|-------|------|-------------|
| `file_path` | string | Path to the source file |
| `start_line` | number | First line of the matched code chunk |
| `end_line` | number | Last line of the matched code chunk |
| `score` | number | Relevance score (0.0-1.0) |
| `language` | string | Programming language |
| `node_type` | string | AST node type (function, class, struct, etc.) |
| `symbol_name` | string or null | Name of the symbol |
| `content` | string | Full code content of the chunk |
