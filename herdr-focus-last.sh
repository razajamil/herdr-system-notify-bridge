#!/usr/bin/env bash
#
# herdr-focus-last: jump to the agent pane that most recently needed attention.
#
# The notify daemon records that pane id in ~/.config/herdr/last-attention-pane
# on every blocked/done transition (whether or not a toast fired). This script
# reads it and does the same thing a notification click does — focus the pane
# and raise the terminal — so a global hotkey (skhd) can action attention
# without touching the mouse, and even after the notification is gone.
set -euo pipefail

STATE="$HOME/.config/herdr/last-attention-pane"
HERDR="$HOME/.local/bin/herdr"
LOG="$HOME/.config/herdr/herdrfocus.log"
TERM_BUNDLE="net.kovidgoyal.kitty"   # kitty; change for another terminal

pane="$(cat "$STATE" 2>/dev/null || true)"
if [ -z "$pane" ]; then
    echo "$(date '+%H:%M:%S') focus-last: no recorded pane" >> "$LOG"
    exit 0
fi

"$HERDR" agent focus "$pane" >> "$LOG" 2>&1 || true
echo "$(date '+%H:%M:%S') focus-last -> $pane" >> "$LOG"
/usr/bin/open -b "$TERM_BUNDLE"
