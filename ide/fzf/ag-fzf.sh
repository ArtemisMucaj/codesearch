#!/usr/bin/env bash
# ag-fzf — live silver-searcher search with fzf, à la Telescope live_grep
#
# Type to search; results update on every keystroke via ag re-execution.
# Press Enter to open the selected file at the matching line in $EDITOR.
#
# Usage:
#   ag-fzf                   # open with empty query
#   ag-fzf "error handling"  # open with a pre-filled query
#
# Dependencies: ag (silversearcher-ag), fzf >= 0.38
# Optional:     bat (syntax-highlighted preview), any $EDITOR (default: vim)

set -euo pipefail

EDITOR="${EDITOR:-vim}"

# Preview: bat with syntax highlighting and line highlight, fall back to cat.
if command -v bat &>/dev/null; then
    PREVIEW_CMD='bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null'
else
    PREVIEW_CMD='cat -n -- {1} 2>/dev/null'
fi

# ag flags used in both reload bindings.
AG_OPTS='--nobreak --noheading --color'

# --disabled     : fzf does no fuzzy matching — ag handles all filtering.
# change:reload  : re-run ag with {q} (current query text) on every keystroke.
# start:reload   : populate the list immediately (including a pre-filled query).
# become(...)    : replace fzf with $EDITOR; cleaner than execute+abort.
fzf \
    --disabled \
    --ansi \
    --query "${*:-}" \
    --prompt 'ag> ' \
    --header 'Type to search  ·  Enter to open  ·  Ctrl-C to quit' \
    --layout reverse \
    --info inline \
    --bind "start:reload:[ -n {q} ] && ag $AG_OPTS -- {q} 2>/dev/null || true" \
    --bind "change:reload:[ -n {q} ] && ag $AG_OPTS -- {q} 2>/dev/null || true" \
    --delimiter ':' \
    --preview "$PREVIEW_CMD" \
    --preview-window 'right,60%,border-left,+{2}+3/3,~3' \
    --bind "enter:become($EDITOR {1} +{2})"
