#!/usr/bin/env bash
#
# herdr-focus-last: jump to the agent that needs you, from a global hotkey.
#
# Under herdr 0.9 the TUI runs inside each client and clients view workspaces
# independently, so `herdr agent focus` only moves SERVER-side focus (and marks
# the agent seen, which clears its badge) -- it cannot move this client's view.
# There is no socket API that can. The only thing that moves the view is a
# client-side keybinding, so this script raises the terminal and types herdr's
# own `open_notification_target` chord into the herdr client via kitty's remote
# control. herdr then does the jump itself.
#
# Requires: kitty `allow_remote_control yes` + `listen_on unix:/tmp/kitty-{kitty_pid}`,
# and a herdr `[ui.toast] delivery` other than "off" -- with toasts off there is
# no notification target for herdr to open, and the chord does nothing.
set -euo pipefail

KITTY="${KITTY_BIN:-/Applications/kitty.app/Contents/MacOS/kitty}"
LOG="$HOME/.config/herdr/herdrfocus.log"
TERM_BUNDLE="net.kovidgoyal.kitty"   # kitty; change for another terminal

# herdr's keys.open_notification_target, default "prefix+o" with prefix ctrl+b.
# Override if you rebound either: HERDR_JUMP_KEYS=$'\002o'
JUMP_KEYS="${HERDR_JUMP_KEYS:-$(printf '\002o')}"

stamp() { date '+%H:%M:%S'; }

# skhd runs us with no kitty environment, so locate the herdr client ourselves:
# kitty listens on unix:/tmp/kitty-<pid>. Prints "<socket> <window-id>".
find_client() {
    local sock
    for sock in /tmp/kitty-*; do
        [ -S "$sock" ] || continue
        "$KITTY" @ --to "unix:$sock" ls 2>/dev/null | python3 -c '
import sys, json
sock = sys.argv[1]
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for osw in data:
    for tab in osw.get("tabs", []):
        for win in tab.get("windows", []):
            for proc in win.get("foreground_processes", []):
                argv = proc.get("cmdline") or []
                if argv and argv[0].rsplit("/", 1)[-1] == "herdr":
                    print(sock, win["id"])
                    sys.exit(0)
sys.exit(1)
' "$sock" && return 0
    done
    return 1
}

client="$(find_client || true)"
if [ -z "$client" ]; then
    echo "$(stamp) focus-last: no herdr client window found in kitty" >> "$LOG"
    exit 0
fi
sock="${client%% *}"
win="${client##* }"

# Raise the terminal first so the jump is visible, then let herdr do it.
/usr/bin/open -b "$TERM_BUNDLE" || true
printf '%s' "$JUMP_KEYS" |
    "$KITTY" @ --to "unix:$sock" send-text --match "id:$win" --stdin
echo "$(stamp) focus-last -> sent open_notification_target to kitty win $win" >> "$LOG"
