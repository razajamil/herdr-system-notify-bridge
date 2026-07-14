#!/usr/bin/env bash
#
# Build and install the herdr system notify bridge end to end:
#   1. build + install the daemon binary
#   2. build + register the herdrfocus:// URL handler (HerdrFocus.app)
#   3. install + load the LaunchAgent
# Idempotent — safe to re-run to update.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="herdr-system-notify-bridge"
DEST="$HOME/.local/bin/$BIN_NAME"
APP="$HOME/.config/herdr/HerdrFocus.app"
PLIST="$HOME/Library/LaunchAgents/dev.local.herdr-notify.plist"
LABEL="dev.local.herdr-notify"
UID_="$(id -u)"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

echo "==> building daemon"
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"
mkdir -p "$HOME/.local/bin"
cp "$SCRIPT_DIR/target/release/$BIN_NAME" "$DEST"
echo "    installed $DEST"

echo "==> building + registering HerdrFocus.app (herdrfocus:// handler)"
rm -rf "$APP"
osacompile -o "$APP" "$SCRIPT_DIR/HerdrFocus.applescript"
APP_PLIST="$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier dev.local.herdrfocus" "$APP_PLIST" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Add :CFBundleIdentifier string dev.local.herdrfocus" "$APP_PLIST"
/usr/libexec/PlistBuddy -c "Delete :CFBundleURLTypes" "$APP_PLIST" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes array" "$APP_PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0 dict" "$APP_PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLName string dev.local.herdrfocus" "$APP_PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes array" "$APP_PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string herdrfocus" "$APP_PLIST"
"$LSREGISTER" -f "$APP"
open "$APP"   # launch once so macOS routes herdrfocus:// URLs to it
echo "    registered herdrfocus:// -> $APP"

echo "==> installing + loading LaunchAgent"
mkdir -p "$HOME/Library/LaunchAgents"
sed "s|__HOME__|$HOME|g" "$SCRIPT_DIR/dev.local.herdr-notify.plist" > "$PLIST"
launchctl bootout "gui/$UID_/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$UID_" "$PLIST"
echo "    loaded $LABEL (pid $(launchctl print "gui/$UID_/$LABEL" 2>/dev/null | awk -F'= ' '/pid =/{print $2; exit}'))"

cat <<EOF

==> Installed. Two manual steps remain:

  1) Disable herdr's own notifications (avoid duplicates) in
     ~/.config/herdr/config.toml:
         [ui.sound]
         enabled = false
         [ui.toast]
         delivery = "off"
     then reload:  herdr server reload-config

  2) System Settings -> Notifications -> terminal-notifier:
         Allow Notifications = ON, Alert style = Alerts

  Logs:   ~/.config/herdr/herdr-notify-bridge.log   (fires)
          ~/.config/herdr/herdrfocus.log            (clicks)
  Status: launchctl print gui/$UID_/$LABEL
EOF
