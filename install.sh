#!/usr/bin/env bash
#
# Build and install the herdr system notify bridge end to end:
#   1. build + install the daemon binary (desktop notifications)
#   2. install the focus-last helper + skhd hotkey (the jump)
#   3. install + load the LaunchAgent
# Idempotent — safe to re-run to update.
#
# There is no herdrfocus:// URL handler any more: under herdr 0.9 a toast click
# cannot move the client's view (see README "Why the click went away").
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="herdr-system-notify-bridge"
DEST="$HOME/.local/bin/$BIN_NAME"
PLIST="$HOME/Library/LaunchAgents/dev.local.herdr-notify.plist"
LABEL="dev.local.herdr-notify"
UID_="$(id -u)"

echo "==> building daemon"
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"
mkdir -p "$HOME/.local/bin"
cp "$SCRIPT_DIR/target/release/$BIN_NAME" "$DEST"
echo "    installed $DEST"

echo "==> installing herdr-focus-last helper"
FOCUS_LAST="$HOME/.local/bin/herdr-focus-last"
install -m 0755 "$SCRIPT_DIR/herdr-focus-last.sh" "$FOCUS_LAST"
echo "    installed $FOCUS_LAST"

echo "==> registering skhd focus-last hotkey"
SKHDRC="$HOME/.config/skhd/skhdrc"
BINDING='cmd + shift - j : ~/.local/bin/herdr-focus-last'
if command -v skhd >/dev/null 2>&1; then
    mkdir -p "$(dirname "$SKHDRC")"
    touch "$SKHDRC"
    if grep -qF 'herdr-focus-last' "$SKHDRC"; then
        echo "    binding already present in $SKHDRC (left as-is)"
    else
        printf '\n# herdr focus-last: jump to the agent that needs attention\n%s\n' "$BINDING" >> "$SKHDRC"
        echo "    added '$BINDING' to $SKHDRC"
    fi
    skhd --restart-service 2>/dev/null || skhd --reload 2>/dev/null || true
    echo "    reloaded skhd"
else
    echo "    skhd not installed — skipping (see manual step 4 below)"
fi

echo "==> installing + loading LaunchAgent"
mkdir -p "$HOME/Library/LaunchAgents"
sed "s|__HOME__|$HOME|g" "$SCRIPT_DIR/dev.local.herdr-notify.plist" > "$PLIST"
launchctl bootout "gui/$UID_/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$UID_" "$PLIST"
echo "    loaded $LABEL (pid $(launchctl print "gui/$UID_/$LABEL" 2>/dev/null | awk -F'= ' '/pid =/{print $2; exit}'))"

cat <<EOF

==> Installed. Manual steps remain:

  1) herdr config (~/.config/herdr/config.toml) — REQUIRED for the hotkey:
         [ui.toast]
         delivery = "herdr"    # MUST NOT be "off": an in-app toast is what
                               # keys.open_notification_target jumps to, and
                               # only "herdr" delivery creates one
         [ui.sound]
         enabled = false       # this daemon plays the sounds
     then reload:  herdr server reload-config

  2) System Settings -> Notifications -> terminal-notifier:
         Allow Notifications = ON, Alert style = Alerts

  3) kitty remote control — REQUIRED for the hotkey, in ~/.config/kitty/kitty.conf:
         allow_remote_control yes
         listen_on unix:/tmp/kitty-{kitty_pid}
     then fully restart kitty.

  4) Global focus-last hotkey (cmd+shift+J). If skhd wasn't installed above:
         brew install koekeishiya/formulae/skhd   # maintainer's tap, not core
         ./install.sh        # re-run to register the binding
         skhd --start-service # first-time service start; grant Accessibility
                              # permission when macOS prompts (System Settings
                              # -> Privacy & Security -> Accessibility -> skhd)
     Rebind the key by editing ~/.config/skhd/skhdrc.

  Logs:   ~/.config/herdr/herdr-notify-bridge.log   (notifications fired)
          ~/.config/herdr/herdrfocus.log            (hotkey presses)
  Status: launchctl print gui/$UID_/$LABEL
EOF
