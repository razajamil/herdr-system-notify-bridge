# herdr-system-notify-bridge

Desktop notifications for [herdr](https://herdr.dev) with a global
jump-to-the-agent hotkey. When an agent in a **background** workspace needs you
(`blocked`) or finishes (`done`), you get a macOS notification; press
`cmd+shift+J` (from any app) and your herdr client jumps to that agent.

## Why this exists

herdr can show its own notifications, but the two things you want — a *desktop*
notification and a keypress that *jumps to the agent* — can't come from herdr
alone:

- `keys.open_notification_target` (`prefix+o`) is the only thing that can move
  the view, and it only has a target when `[ui.toast] delivery = "herdr"`,
  i.e. an **in-app** toast. That tells you nothing while you're in a browser.
- With `delivery = "system"` or `"terminal"` you get a desktop notification but
  **no jump target** — `prefix+o` does nothing (verified on 0.9.0).

So the two halves are split: herdr shows the in-app toast that gives `prefix+o`
its target, this daemon fires the desktop notification, and the hotkey types
`prefix+o` into the herdr client for you.

## Why the click went away

Up to herdr 0.8 the toast itself was clickable: it carried
`-open herdrfocus://focus/<pane>`, and a small `HerdrFocus.app` URL handler
turned the click into `herdr agent focus <pane>`.

**herdr 0.9 killed that, and nothing can bring it back.** 0.9 moved the terminal
UI inside each client, and clients view workspaces independently, so
`herdr agent focus`:

- sets **server-side** focus and marks the agent **seen** (its badge clears), but
- does **not** move any client's view.

herdr's socket API has no method to move a client's view — only a client-side
keybinding does. So a toast click, or any external process, is structurally
unable to jump to a pane. The `herdrfocus://` handler and `HerdrFocus.app` are
gone; the hotkey drives the client instead, via kitty's remote control.

The toast is therefore informational: it says which pane wants you and reminds
you of the hotkey.

## How it works

```
 herdr server ── unix socket (events) ──▶  this daemon
                                            │  agent enters blocked/done
                                            │  in a NON-focused workspace
                                            ▼
                                   terminal-notifier
                                   (macOS toast — informational, not clickable)

 you press cmd+shift+J  ──▶  skhd  ──▶  herdr-focus-last
                                            │  raise kitty, then
                                            │  kitty @ send-text $'\x02o'
                                            ▼
                                   herdr client  (prefix+o =
                                   keys.open_notification_target)
                                   → the client moves its own view
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
| **daemon** (this crate) | `~/.local/bin/herdr-system-notify-bridge` | Watches herdr, fires the desktop toast + sound |
| **focus-last helper** | `~/.local/bin/herdr-focus-last` | Raises kitty and types `prefix+o` into the herdr client (hotkey target) |
| **hotkey** | `~/.config/skhd/skhdrc` (skhd) | Binds `cmd+shift+J` → `herdr-focus-last` |
| **LaunchAgent** | `~/Library/LaunchAgents/dev.local.herdr-notify.plist` | Runs the daemon at login, keeps it alive |

The daemon keeps no state of its own: herdr picks the jump target, so which
agent you land on is herdr's notion of the current notification target.

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
  tap, not homebrew-core; needed for the global hotkey)
- **kitty with remote control enabled** — the hotkey types into the herdr
  client through it, so `~/.config/kitty/kitty.conf` needs:
  ```
  allow_remote_control yes
  listen_on unix:/tmp/kitty-{kitty_pid}
  ```
  (then fully restart kitty). Another terminal works only if it can inject
  keystrokes into a window from outside; change `TERM_BUNDLE` and the send
  command in `herdr-focus-last.sh`.
- **`[ui.toast] delivery = "herdr"` in herdr's config** — anything else (and
  especially `"off"`) leaves `prefix+o` with no target, and the hotkey silently
  does nothing.
- Rust toolchain to build (`cargo`)

## Setup

**Quick install** — build and install the daemon, the hotkey helper, the skhd
binding and the LaunchAgent in one shot:

```sh
./install.sh
```

Then do the manual steps it prints (steps 3–5 below). The rest of this section
documents what `install.sh` does, for reference or manual setup.

### 1. Build & install the daemon

```sh
cargo build --release
cp target/release/herdr-system-notify-bridge ~/.local/bin/
```

### 2. Enable kitty remote control

The hotkey jumps by typing herdr's own `prefix+o` into the herdr client, which
needs kitty's remote control. In `~/.config/kitty/kitty.conf`:

```
allow_remote_control yes
listen_on unix:/tmp/kitty-{kitty_pid}
```

Then **fully restart kitty** (config reload does not open the socket).

The helper finds the client itself — it scans `/tmp/kitty-*` for the window
whose foreground process is `herdr` — because skhd runs it with no kitty
environment variables.

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

### 4. herdr config (required — this is what makes the hotkey work)

In `~/.config/herdr/config.toml`:

```toml
[ui.toast]
delivery = "herdr"   # in-app toast; this is what prefix+o jumps to.
                     # "off", "system" and "terminal" all leave prefix+o
                     # with no target, and the hotkey does nothing.
[ui.sound]
enabled = false      # this daemon plays the sounds, so herdr's stay off
```

Then `herdr server reload-config`.

You will see two things per event: herdr's small in-app toast (which powers the
jump) and this daemon's desktop notification. That duplication is the price of
having both a desktop alert and a working jump — see **Why this exists**.

### 5. macOS notification permission

System Settings → Notifications → **terminal-notifier** → Allow Notifications on,
**Alert style = Alerts** (banners auto-dismiss; alerts persist). Otherwise
notifications only land silently in Notification Center.

### 6. Global hotkey (skhd)

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

You can test the helper without a hotkey at all — including in a bare
environment, the way skhd will run it:

```sh
env -i HOME="$HOME" PATH=/usr/bin:/bin ~/.local/bin/herdr-focus-last
```

It logs what it did to `~/.config/herdr/herdrfocus.log`. If it logs
`no herdr client window found in kitty`, remote control isn't enabled (step 2).
If it logs a successful send but nothing moves, herdr has no notification
target — check `[ui.toast] delivery` (step 4).

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
tail -f ~/.config/herdr/herdrfocus.log               # what the hotkey did
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
| `HOTKEY_HINT` | `cmd+shift+J` | Shown in the toast body; keep in step with the skhd binding |

Per-status sounds (`Sosumi` for blocked, `Glass` for done) are in `notify()`.

The hotkey side is tuned in `herdr-focus-last.sh`: `KITTY_BIN`, `TERM_BUNDLE`,
and `HERDR_JUMP_KEYS` (default `$'\002o'` = `prefix+o`) — change the last one
if you rebound herdr's `prefix` or `open_notification_target`.

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

- **The toast is not clickable.** Clicking it does nothing at all — the hotkey
  is the only way to jump. See **Why the click went away**.
- **herdr picks the target, not this daemon.** The hotkey goes wherever herdr's
  notification target points, which is generally the most recent one. There's no
  cycling through several waiting agents; for that, bind herdr's own
  `next_agent`/`previous_agent` (unset by default) in `[keys]`.
- **You get two alerts per event** — herdr's in-app toast plus this daemon's
  desktop one. Unavoidable if you want both a desktop alert and a working jump.
- **Duplicate herdr clients:** the helper types into the first kitty window it
  finds running `herdr`. With several herdr clients open it may drive the wrong
  one.
- Assumes **kitty** as the terminal, and needs its remote control enabled — the
  hotkey injects keystrokes through it. Other terminals need an equivalent
  mechanism; change `TERM_BUNDLE` and the send command in
  `herdr-focus-last.sh`.
- Paths to `terminal-notifier`, `kitty` and the binary are effectively
  hardcoded for a Homebrew/`~/.local/bin`/`/Applications` layout.
