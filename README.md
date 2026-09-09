# herdr-system-notify-bridge

Click-to-focus desktop notifications for [herdr](https://herdr.dev). When an
agent in a **background** workspace needs you (`blocked`) or finishes (`done`),
you get a macOS notification — and **clicking it jumps you straight to that
exact pane**.

There's also a **global hotkey** (`cmd+shift+J` by default): press it to jump
to the agent that most recently needed attention — with no mouse, and whether or
not there's a live notification. macOS gives third-party apps no way to bind a
key to a notification banner itself, so the daemon instead records the last
attention pane to a state file and the hotkey focuses it. That means it also
works when the toast is gone, was dismissed, or never fired (the agent's own
workspace was focused at the time).

## Why this exists

herdr can't do click-to-pane itself on modern macOS (Sequoia/Tahoe):

- its built-in system notifications use `terminal-notifier -activate`, which
  only focuses the **app**, not a specific pane;
- `terminal-notifier`'s `-execute` (run-a-command-on-click) is **broken** on
  modern macOS (deprecated `NSUserNotification`).

But `terminal-notifier -open <url>` **does** still fire on click, and
`herdr agent focus <pane>` works. This bridge stitches those together: it fires
notifications wired with `-open herdrfocus://focus/<pane>`, and a tiny URL
handler turns that click into `herdr agent focus <pane>`.

## How it works

```
 herdr server ── unix socket (events) ──▶  this daemon
                                            │  agent enters blocked/done
                                            │  in a NON-focused workspace
                                            ▼
                                   terminal-notifier
                                   (macOS toast, -open herdrfocus://focus/<pane>)
                                            │
                                     you click it
                                            ▼
                                   HerdrFocus.app  (herdrfocus:// URL handler)
                                   → `herdr agent focus <pane>` + raise the terminal
```

The daemon connects to the herdr socket, `events.subscribe`s to
`pane.agent_status_changed` for every agent pane, and reacts to the event
stream in real time (≈1s, no polling). On each `blocked`/`done` transition it
takes a fresh `session.snapshot` to check whether that pane's workspace is
focused; if not, it fires the toast. New agent panes arrive as
`pane.created`/`pane.agent_detected` events and are picked up at once. It
auto-reconnects if the herdr server restarts.

A subscription can't be extended once it is live (see the protocol notes
below), so adopting a new pane means reconnecting with a wider subscription.
The daemon carries each pane's last-known status across that reconnect and
diffs it against a snapshot taken *after* the new subscription is live, so a
transition that lands in the gap still fires. A 60s snapshot resync backs that
up in case the stream ever drops an event.

## Components

| Piece | Where | Role |
|---|---|---|
| **daemon** (this crate) | `~/.local/bin/herdr-system-notify-bridge` | Watches herdr, fires click-to-focus toasts, records the last attention pane |
| **URL handler** | `~/.config/herdr/HerdrFocus.app` | Owns `herdrfocus://`; runs `herdr agent focus <pane>` on click |
| **focus-last helper** | `~/.local/bin/herdr-focus-last` | Reads `last-attention-pane`, focuses it + raises the terminal (hotkey target) |
| **hotkey** | `~/.config/skhd/skhdrc` (skhd) | Binds `cmd+shift+J` → `herdr-focus-last` |
| **LaunchAgent** | `~/Library/LaunchAgents/dev.local.herdr-notify.plist` | Runs the daemon at login, keeps it alive |

The daemon writes the most-recent blocked/done pane id to
`~/.config/herdr/last-attention-pane` on every attention transition (before
deciding whether to toast), which is what the hotkey reads.

## Requirements

- macOS (arm64 or x86_64)
- [herdr](https://herdr.dev) **0.9+** running (`~/.config/herdr/herdr.sock`
  present). The socket protocol changed in 0.9 in ways that need the client to
  cooperate — see the protocol notes at the end. This daemon targets 0.9
  (protocol 22) and subscribes to pane-lifecycle events that 0.8 doesn't
  offer, so it is not expected to work against 0.8.
- `terminal-notifier` — `brew install terminal-notifier` (expected at
  `/opt/homebrew/bin/terminal-notifier`)
- `skhd` — `brew install koekeishiya/formulae/skhd` (it's in the maintainer's
  tap, not homebrew-core; for the global focus-last hotkey — optional if you
  only want click-to-focus)
- Rust toolchain to build (`cargo`)

## Setup

**Quick install** — build, install the binary, register the URL handler, and
load the LaunchAgent in one shot:

```sh
./install.sh
```

Then do the two manual steps it prints (steps 4 & 5 below). The rest of this
section documents what `install.sh` does, for reference or manual setup.

### 1. Build & install the daemon

```sh
cargo build --release
cp target/release/herdr-system-notify-bridge ~/.local/bin/
```

### 2. Install the `herdrfocus://` URL handler

Create `~/.config/herdr/HerdrFocus.app` from this AppleScript (pane id is taken
as everything after the last `/`, so the `:` in pane ids like `w1:p4` is safe):

```applescript
on open location this_URL
	set AppleScript's text item delimiters to "/"
	set parts to text items of this_URL
	set paneId to last item of parts
	set AppleScript's text item delimiters to ""
	if paneId is "" then return
	set herdr to (POSIX path of (path to home folder)) & ".local/bin/herdr"
	do shell script quoted form of herdr & " agent focus " & quoted form of paneId & " ; /usr/bin/open -b net.kovidgoyal.kitty"
end open location
```

Compile and register it:

```sh
osacompile -o ~/.config/herdr/HerdrFocus.app HerdrFocus.applescript
# declare the URL scheme
plist=~/.config/herdr/HerdrFocus.app/Contents/Info.plist
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier dev.local.herdrfocus" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes array" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0 dict" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLName string dev.local.herdrfocus" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes array" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string herdrfocus" "$plist"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f ~/.config/herdr/HerdrFocus.app
open ~/.config/herdr/HerdrFocus.app   # launch once to clear macOS first-run
```

> The app must be launched once (or have its quarantine attribute cleared)
> before macOS will route `herdrfocus://` URLs to it.

### 3. Install the LaunchAgent

`~/Library/LaunchAgents/dev.local.herdr-notify.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>dev.local.herdr-notify</string>
    <key>ProgramArguments</key>
    <array><string>/Users/YOU/.local/bin/herdr-system-notify-bridge</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
</dict>
</plist>
```

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.local.herdr-notify.plist
```

### 4. Turn off herdr's own notifications (avoid duplicates)

In `~/.config/herdr/config.toml`:

```toml
[ui.sound]
enabled = false
[ui.toast]
delivery = "off"
```

### 5. macOS notification permission

System Settings → Notifications → **terminal-notifier** → Allow Notifications on,
**Alert style = Alerts** (banners auto-dismiss; alerts persist). Otherwise
notifications only land silently in Notification Center.

### 6. Global focus-last hotkey (skhd)

`install.sh` installs the `herdr-focus-last` helper to `~/.local/bin` and, if
`skhd` is present, adds this binding to `~/.config/skhd/skhdrc`:

```
cmd + shift - j : ~/.local/bin/herdr-focus-last
```

If skhd wasn't installed when you first ran `install.sh`:

```sh
brew install koekeishiya/formulae/skhd   # in the maintainer's tap, not core
./install.sh              # re-run: registers the binding, reloads skhd
skhd --start-service      # first-time service start
```

On first start macOS will prompt for Accessibility permission — grant it under
**System Settings → Privacy & Security → Accessibility → skhd**, or the hotkey
silently won't fire. Rebind the key by editing `~/.config/skhd/skhdrc` (see
`skhd-herdr-focus.conf` for the syntax) and running `skhd --restart-service`.

You can test the helper without a hotkey at all:

```sh
~/.local/bin/herdr-focus-last   # jumps to the last agent that needed attention
```

## Managing it

```sh
# restart (e.g. after reinstalling the binary)
launchctl kickstart -k gui/$(id -u)/dev.local.herdr-notify
# stop (returns at next login)
launchctl bootout gui/$(id -u)/dev.local.herdr-notify
# status
launchctl print gui/$(id -u)/dev.local.herdr-notify
# logs
tail -f ~/.config/herdr/herdr-notify-bridge.log      # what the daemon fired
tail -f ~/.config/herdr/herdrfocus.log               # what clicks focused
```

Update after editing the code:

```sh
cargo build --release && cp target/release/herdr-system-notify-bridge ~/.local/bin/
launchctl kickstart -k gui/$(id -u)/dev.local.herdr-notify
```

## Tuning

Constants at the top of `src/main.rs`:

| Constant | Default | Meaning |
|---|---|---|
| `ATTENTION` | `["blocked", "done"]` | Which agent states trigger a notification |
| `RESYNC` | `60s` | Safety-net snapshot diff, in case the stream drops an event |
| `LIFECYCLE` | `pane.created`, `pane.agent_detected`, `pane.closed`, `pane.exited` | Pane-lifecycle events subscribed alongside the per-pane status ones |
| `TERMINAL_NOTIFIER` | `/opt/homebrew/bin/terminal-notifier` | Notifier path |

Per-status sounds (`Sosumi` for blocked, `Glass` for done) are in `notify()`.

## herdr socket protocol (reference)

Verified against a live `~/.config/herdr/herdr.sock` on **herdr 0.9.0,
protocol 22**. `herdr api schema --json` dumps the whole thing.

- **Framing:** newline-delimited JSON.
- **Request:** `{"id","method","params"}` — `params` is required even for `ping`.
- **Snapshot:** method `session.snapshot` → `result.snapshot`
  (`focused_workspace_id`, `workspaces[]`, `agents[]`).
- **Subscribe:** `events.subscribe`, params
  `{"subscriptions":[{"type":"pane.agent_status_changed","pane_id":"w1:p4"}]}`.
  Acked with `{"result":{"type":"subscription_started"}}`.
- **Event (status probe, dotted kind):**
  `{"event":"pane.agent_status_changed","data":{"agent","agent_status","pane_id","workspace_id"}}`.
- **Event (lifecycle, snake_case kind):**
  `{"event":"pane_created","data":{"type":"pane_created","pane":{…}}}`;
  `pane_closed`/`pane_exited` carry `pane_id` + `workspace_id` directly.
  Note the two envelopes use different naming for the same concept — the
  *subscription* is `pane.created`, the *event* that arrives is `pane_created`.
- Agent states: `idle`, `working`, `blocked`, `done`, `unknown`. A Claude turn
  *finishing* is `idle` (not an attention state); `blocked` = waiting on a
  prompt/question; opencode completion is `done`. Reporting `idle` on a pane
  that is currently `blocked` yields `done`, not `idle`.

Three behaviours that constrain any client (all three broke this daemon on the
0.8 → 0.9 upgrade):

- **`events.subscribe` is terminal for its connection.** Once it is acked, the
  socket is a one-way event stream: *any* further request on it — a second
  `events.subscribe`, even a `ping` — makes the server close the connection.
  Subscribe once per connection, and use a separate connection for
  `session.snapshot` and everything else.
- **One bad `pane_id` fails the whole batch.** A pane that closed between the
  snapshot and the subscribe gets you
  `{"error":{"code":"pane_not_found"},"id":"<req>:sub:<index>:probe"}` and the
  connection closes — no subscription at all, not a partial one. Check the ack;
  the offending entry's batch index is in the error `id`.
- **No history replay.** Per the 0.9 notes, "new lifecycle event subscriptions
  now start with live events rather than replaying retained history. API
  clients should subscribe before taking their initial snapshot to avoid
  missing changes." Anything that happens before the subscription goes live is
  gone unless you diff it back out of a snapshot yourself.

Subscribable types (27 in protocol 22): `workspace.{created,updated,
metadata_updated,renamed,moved,reordered,closed,focused}`,
`worktree.{created,opened,removed}`, `tab.{created,closed,focused,renamed,moved}`,
`pane.{created,closed,updated,focused,moved,exited,agent_detected,
output_matched,agent_status_changed,scroll_changed}`, `layout.updated`.
Only `pane.agent_status_changed`, `pane.output_matched` and
`pane.scroll_changed` take a `pane_id` (required — there is no wildcard); the
rest are global.

## Caveats

- **Split tabs:** if an agent shares its tab with another pane, `agent focus`
  sets the server focus onto the agent pane, but you'll see the whole split.
  To land unambiguously, `notify()` could additionally run
  `herdr pane zoom <pane> --on`.
- Assumes **kitty** as the terminal (the URL handler raises
  `net.kovidgoyal.kitty`). Change the bundle id in the AppleScript for other
  terminals.
- Paths to `terminal-notifier` and the binary are effectively hardcoded for a
  Homebrew/`~/.local/bin` layout.
