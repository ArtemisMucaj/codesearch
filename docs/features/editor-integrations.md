# Editor Integrations

CodeSearch provides output formats and plugins for integrating semantic search into your editor workflow.

## Output Formats

The `--format` (`-F`) flag on the `search` command controls output:

| Format | Description |
|--------|-------------|
| `text` | Human-readable output with code previews (default) |
| `json` | Structured JSON array for programmatic consumption |
| `vimgrep` | `file:line:col:text` for Neovim quickfix list and Telescope |

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

Restart Zed and open the AI assistant — the server will be listed in the context-server panel. The assistant can then call `search_code`, `analyze_impact`, and `get_symbol_context` autonomously while you chat.

### Tasks (command palette integration)

Tasks let you run searches from Zed's command palette (`cmd-shift-p` → "task: spawn") and display results in the terminal panel. They use Zed's built-in task variables:

| Variable | Value |
|---|---|
| `$ZED_SELECTED_TEXT` | Currently selected text |
| `$ZED_SYMBOL` | Symbol under the cursor (from the language server) |
| `$ZED_WORKTREE_ROOT` | Absolute path of the project root |

Run [`ide/zed/setup.sh`](../../ide/zed/setup.sh) for an automated install, or manually copy [`ide/zed/tasks.json`](../../ide/zed/tasks.json) to your project's `.zed/tasks.json` (or merge it into `~/.config/zed/tasks.json` for a global install).

#### Keybindings

Copy [`ide/zed/keybindings.json`](../../ide/zed/keybindings.json) into `~/.config/zed/keymap.json` to add keyboard shortcuts (`ctrl-shift-f` search, `ctrl-shift-i` impact, `ctrl-shift-x` context), or let `setup.sh` handle it interactively.

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
