#!/usr/bin/env bash
# setup.sh — Install CodeSearch (+ Television) integration into Zed
#
# What it does:
#   1. Merges codesearch and television tasks into ~/.config/zed/tasks.json
#   2. Optionally adds keybindings to ~/.config/zed/keymap.json
#   3. Merges codesearch cable channels into ~/.config/television/cable.toml
#
# Requirements: jq (brew install jq | apt install jq)
# Optional:     tv  (https://github.com/alexpasmantier/television)

set -euo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; BOLD='\033[1m'; NC='\033[0m'
info() { printf "${GREEN}[+]${NC} %s\n" "$*"; }
warn() { printf "${YELLOW}[!]${NC} %s\n" "$*"; }
die()  { printf "${RED}[x]${NC} %s\n" "$*" >&2; exit 1; }

command -v jq &>/dev/null \
    || die "'jq' is required — install it with:  brew install jq   OR   apt install jq"

ZED_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/zed"
TV_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/television"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

mkdir -p "$ZED_DIR"

command -v codesearch &>/dev/null \
    || warn "'codesearch' not found on PATH — install it before starting Zed."

command -v tv &>/dev/null \
    || warn "'tv' (television) not found on PATH — install it from https://github.com/alexpasmantier/television; all codesearch tasks require it."

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

# ── 2. Television cable channels ── cable.toml ───────────────────────────────
if command -v tv &>/dev/null; then
    mkdir -p "$TV_DIR"
    CABLE_FILE="$TV_DIR/cable.toml"
    CABLE_SRC="$REPO_ROOT/ide/tv/cable.toml"

    if [[ -f "$CABLE_FILE" ]]; then
        # Append channels that are not already present (match by name = "..." line).
        while IFS= read -r line; do
            channel_name=$(echo "$line" | grep -oP '(?<=name = ")[^"]+' || true)
            if [[ -n "$channel_name" ]] && ! grep -q "name = \"$channel_name\"" "$CABLE_FILE"; then
                printf '\n' >> "$CABLE_FILE"
                # Append the full channel block (from this name line to next [[)
                awk "/name = \"$channel_name\"/{found=1; print; next} found && /^\[\[/{exit} found{print}" "$CABLE_SRC" >> "$CABLE_FILE"
                info "Cable channel '$channel_name' added → $CABLE_FILE"
            elif [[ -n "$channel_name" ]]; then
                info "Cable channel '$channel_name' already present — skipped."
            fi
        done < <(grep 'name = "' "$CABLE_SRC")
    else
        cp "$CABLE_SRC" "$CABLE_FILE"
        info "Cable channels installed → $CABLE_FILE"
    fi
fi

# ── 3. Keybindings ── keymap.json (optional) ──────────────────────────────────
printf "\n${BOLD}Suggested keybindings:${NC}\n"
printf "  ctrl-shift-f  →  codesearch: search         (tv cable channel)\n"
printf "  ctrl-shift-i  →  codesearch: impact analysis (pipes through tv)\n"
printf "  ctrl-shift-x  →  codesearch: symbol context  (pipes through tv)\n"
printf "  ctrl-shift-t  →  television: find file\n\n"
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
info "Done — restart Zed (and tv) to pick up the changes."
