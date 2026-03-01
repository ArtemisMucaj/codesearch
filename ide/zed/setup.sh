#!/usr/bin/env bash
# setup.sh — Install CodeSearch + fzf integration into Zed
#
# What it does:
#   1. Merges codesearch and fzf tasks into ~/.config/zed/tasks.json
#   2. Optionally adds keybindings to ~/.config/zed/keymap.json
#
# Requirements: jq (brew install jq | apt install jq)

set -euo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; BOLD='\033[1m'; NC='\033[0m'
info() { printf "${GREEN}[+]${NC} %s\n" "$*"; }
warn() { printf "${YELLOW}[!]${NC} %s\n" "$*"; }
die()  { printf "${RED}[x]${NC} %s\n" "$*" >&2; exit 1; }

command -v jq &>/dev/null \
    || die "'jq' is required — install it with:  brew install jq   OR   apt install jq"

ZED_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/zed"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$ZED_DIR"

command -v codesearch &>/dev/null \
    || warn "'codesearch' not found on PATH — install it before starting Zed."

command -v fzf &>/dev/null \
    || warn "'fzf' not found on PATH — install it (brew install fzf / apt install fzf)."

# Atomically write a file: stage to a temp file, then rename into place.
write_file() {
    local dest="$1" content="$2"
    local tmp
    tmp=$(mktemp "${dest}.XXXXXX")
    printf '%s\n' "$content" > "$tmp"
    mv "$tmp" "$dest"
}

# ── 1. Zed tasks ── tasks.json ───────────────────────────────────────────────
TASKS_FILE="$ZED_DIR/tasks.json"
[[ -f "$TASKS_FILE" ]] || printf '[]\n' > "$TASKS_FILE"

new_tasks=$(jq '[.[] | select(.label)]' "$HERE/tasks.json")
existing_tasks=$(cat "$TASKS_FILE")
merged_tasks=$(jq -n \
    --argjson existing "$existing_tasks" \
    --argjson new      "$new_tasks" \
    '$existing + ($new | map(
        select(.label as $l | ($existing | map(.label) | index($l)) == null)
    ))')
write_file "$TASKS_FILE" "$merged_tasks"
info "Tasks merged → $TASKS_FILE"

# ── 2. Keybindings ── keymap.json (optional) ──────────────────────────────────
printf "\n${BOLD}Suggested keybindings:${NC}\n"
printf "  ctrl-shift-f  →  codesearch: search         (prompt + fzf picker)\n"
printf "  ctrl-shift-i  →  codesearch: impact analysis (fzf picker)\n"
printf "  ctrl-shift-x  →  codesearch: symbol context  (fzf picker)\n\n"
read -r -p "Add these keybindings to keymap.json? [y/N] " yn

if [[ "$yn" == [yY]* ]]; then
    KEYMAP="$ZED_DIR/keymap.json"
    [[ -f "$KEYMAP" ]] || printf '[]\n' > "$KEYMAP"
    new_keys=$(jq '.' "$HERE/keybindings.json")
    existing_keys=$(cat "$KEYMAP")
    merged_keys=$(jq -n --argjson e "$existing_keys" --argjson n "$new_keys" '$e + $n')
    write_file "$KEYMAP" "$merged_keys"
    info "Keybindings added → $KEYMAP"
else
    info "Keybindings skipped."
fi

printf "\n"
info "Done — restart Zed to pick up the changes."
