#!/usr/bin/env bash
# setup.sh — install ag-fzf and codesearch-fzf to a directory on $PATH
#
# Usage:
#   ./setup.sh                   # installs to ~/.local/bin (default)
#   ./setup.sh /usr/local/bin    # installs to a custom directory

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${1:-$HOME/.local/bin}"

check_dep() {
    local cmd="$1"
    local install_hint="${2:-}"
    if command -v "$cmd" &>/dev/null; then
        printf '  \e[32mOK\e[0m  %s (%s)\n' "$cmd" "$(command -v "$cmd")"
    else
        printf '  \e[33mMISS\e[0m %s — not found' "$cmd"
        [[ -n "$install_hint" ]] && printf '  (install: %s)' "$install_hint"
        printf '\n'
    fi
}

echo "==> Checking dependencies"
check_dep ag        "apt install silversearcher-ag  / brew install the_silver_searcher"
check_dep fzf       "apt install fzf               / brew install fzf"
check_dep bat       "apt install bat               / brew install bat  (optional)"
check_dep codesearch ""
check_dep jq        "apt install jq                / brew install jq"

echo ""
echo "==> Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

for script in ag-fzf.sh codesearch-fzf.sh; do
    name="${script%.sh}"
    target="$INSTALL_DIR/$name"
    chmod +x "$SCRIPT_DIR/$script"
    ln -sf "$SCRIPT_DIR/$script" "$target"
    printf '  linked  %s\n' "$target"
done

echo ""
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    echo "NOTE: $INSTALL_DIR is not in your PATH."
    echo "Add this to ~/.bashrc or ~/.zshrc:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
else
    echo "Done. Both commands are ready:"
    echo "  ag-fzf             — live ag search (à la Telescope live_grep)"
    echo "  codesearch-fzf     — semantic search + fzf result picker"
fi
