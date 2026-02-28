#!/usr/bin/env bash
# codesearch-fzf — semantic code search with fzf result picker
#
# Runs codesearch once for a query, then lets you browse and filter results
# with fzf. Press Enter to open the selected chunk in $EDITOR at the right line.
#
# Semantic search cannot re-run per keystroke (embedding computation is too
# slow for live reload). The fzf input here fuzzy-filters the returned results
# by filename, symbol name, and content — useful when the result set is large.
#
# Usage:
#   codesearch-fzf                       # prompts for a query
#   codesearch-fzf "error handling"      # run immediately with given query
#
# Environment:
#   EDITOR          editor to open results in           (default: vim)
#   CODESEARCH_BIN  path to codesearch binary           (default: codesearch)
#   CODESEARCH_NUM  number of results to fetch          (default: 20)
#   CODESEARCH_ARGS extra codesearch flags, space-sep.  (default: empty)
#
# Dependencies: codesearch, fzf >= 0.38, jq
# Optional:     bat (syntax-highlighted preview)

set -euo pipefail

EDITOR="${EDITOR:-vim}"
CS="${CODESEARCH_BIN:-codesearch}"
NUM="${CODESEARCH_NUM:-20}"
EXTRA_ARGS="${CODESEARCH_ARGS:-}"

if command -v bat &>/dev/null; then
    PREVIEW_CMD='bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null'
else
    PREVIEW_CMD='cat -n -- {1} 2>/dev/null'
fi

# Resolve query: from args or interactive prompt.
QUERY="${*:-}"
if [[ -z "$QUERY" ]]; then
    read -rp "Semantic search: " QUERY
fi
[[ -z "$QUERY" ]] && exit 0

# Emit one tab-delimited line per result:
#   file_path \t start_line \t score \t symbol \t content_snippet
# Tab is used as delimiter because colons are common in code content.
# shellcheck disable=SC2086
"$CS" search "$QUERY" --format json --num "$NUM" $EXTRA_ARGS \
    | jq -r '
        .[] |
        [
          .file_path,
          (.start_line | tostring),
          (.score * 1000 | round / 1000 | tostring),
          (.symbol_name // .node_type // ""),
          (.content | gsub("[\\n\\t\\r]"; " ") | .[0:100])
        ] | join("\t")
      ' \
    | fzf \
        --ansi \
        --delimiter $'\t' \
        --with-nth '3,4,1,5' \
        --preview "$PREVIEW_CMD" \
        --preview-window 'right,60%,border-left,+{2}+3/3,~3' \
        --prompt 'codesearch> ' \
        --header "Query: $QUERY  ·  Enter to open  ·  Ctrl-C to quit" \
        --layout reverse \
        --bind "enter:become($EDITOR {1} +{2})"
