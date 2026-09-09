//! herdr system notify bridge (event-driven, herdr 0.9 protocol).
//!
//! Connects to the herdr socket, subscribes to `pane.agent_status_changed`
//! events for every agent pane, and — when a pane enters `blocked`/`done`
//! while its workspace is NOT focused — fires a macOS desktop notification.
//!
//! This exists only because herdr's own desktop delivery can't drive the jump:
//! `[ui.toast] delivery` has to be `"herdr"` (an in-app toast) for
//! `keys.open_notification_target` to have a target, and that gives no
//! notification at all when you're in another app. So herdr shows the in-app
//! toast that powers the jump, and this daemon supplies the desktop one.
//!
//! The toast is NOT clickable. Up to herdr 0.8 it carried
//! `-open herdrfocus://focus/<pane>`, and the click ran `herdr agent focus`.
//! Under 0.9 the terminal UI runs inside each client and clients view
//! workspaces independently, so `agent focus` only moves SERVER-side focus and
//! marks the agent seen — it cannot move a client's view, and no socket API
//! can. Only a client keybinding does, so jumping now goes through
//! `herdr-focus-last`, which types herdr's own `open_notification_target`
//! chord into the client with kitty's remote control.
//!
//! Protocol (herdr 0.9, protocol 22 — verified against the live socket):
//!   framing  : newline-delimited JSON; request {"id","method","params"}
//!   snapshot : method "session.snapshot" -> result.snapshot
//!   subscribe: method "events.subscribe", params.subscriptions=[{type,..}]
//!   probe events (dotted kind):    {"event":"pane.agent_status_changed",
//!                                   "data":{agent,agent_status,pane_id,workspace_id}}
//!   lifecycle events (snake kind): {"event":"pane_created",
//!                                   "data":{"type":"pane_created","pane":{..}}}
//!
//! Three 0.9 behaviours shape the design here — all three broke the pre-0.9
//! version of this daemon, which subscribed again on a live stream every 15s:
//!
//!  1. `events.subscribe` is TERMINAL for its connection. Once acked, the
//!     socket is a one-way event stream and ANY further request on it (even a
//!     `ping`) makes the server close it. So we subscribe exactly once per
//!     connection and never write to that socket again; picking up new panes
//!     means a fresh connection, not a second subscribe.
//!  2. A single unknown `pane_id` fails the WHOLE batch (`pane_not_found`) and
//!     closes the connection — so we check the ack, drop the offending entry
//!     (its batch index is encoded in the error id) and retry.
//!  3. Subscriptions no longer replay retained history ("subscribe before
//!     taking your initial snapshot"), so any gap in the stream silently loses
//!     transitions. We subscribe FIRST, then take the baseline snapshot, and
//!     diff it against the statuses carried over from before the gap to
//!     recover transitions that the stream never delivered.
//!
//! 0.9 also added subscribable pane lifecycle events, so new agent panes are
//! now discovered from `pane.created`/`pane.agent_detected` instead of the old
//! 15s rediscovery poll. The periodic snapshot remains only as a safety net.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

const TERMINAL_NOTIFIER: &str = "/opt/homebrew/bin/terminal-notifier";
/// Shown in the toast body, since the toast itself is no longer clickable.
/// Keep in step with the skhd binding in `skhd-herdr-focus.conf`.
const HOTKEY_HINT: &str = "cmd+shift+J";
const ATTENTION: [&str; 2] = ["blocked", "done"];
/// Safety-net reconcile. Events drive discovery now, so this only catches what
/// the stream may have dropped. It never writes to the event stream.
const RESYNC: Duration = Duration::from_secs(60);
/// Pane-independent lifecycle subscriptions, sent in the same batch as the
/// per-pane status ones. `created`/`agent_detected` say a new pane may need a
/// status subscription; `closed`/`exited` let us forget dead panes before the
/// next subscribe (one dead pane_id would fail the whole batch).
const LIFECYCLE: [&str; 4] = [
    "pane.created",
    "pane.agent_detected",
    "pane.closed",
    "pane.exited",
];

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/Users/raza.jamil".to_string())
}
fn sock_path() -> String {
    format!("{}/.config/herdr/herdr.sock", home())
}
fn log_path() -> String {
    format!("{}/.config/herdr/herdr-notify-bridge.log", home())
}
fn now() -> String {
    Command::new("/bin/date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "{} {}", now(), msg);
    }
}

/// One-shot request/response on a fresh connection; returns `result.snapshot`.
/// Always a separate connection — the event stream cannot carry requests.
fn snapshot() -> Option<Value> {
    let mut s = UnixStream::connect(sock_path()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(b"{\"id\":\"snap\",\"method\":\"session.snapshot\",\"params\":{}}\n")
        .ok()?;
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    v.get("result")?.get("snapshot").cloned()
}

/// What we need about an agent pane to be able to notify about it.
struct Row {
    status: String,
    ws: String,
    agent: String,
}

/// Everything that has to survive a reconnect.
#[derive(Default)]
struct State {
    /// Last-known `agent_status` per pane. Carried across reconnects so a
    /// snapshot diff can recover transitions the event stream never delivered.
    prev: HashMap<String, String>,
    /// Whether we've taken our first baseline (before which we adopt statuses
    /// silently instead of toasting a whole session of already-blocked agents).
    seeded: bool,
}

fn agent_rows(snap: &Value) -> HashMap<String, Row> {
    let mut m = HashMap::new();
    if let Some(agents) = snap.get("agents").and_then(|a| a.as_array()) {
        for a in agents {
            let pane = a.get("pane_id").and_then(|x| x.as_str());
            let status = a.get("agent_status").and_then(|x| x.as_str());
            if let (Some(pane), Some(status)) = (pane, status) {
                m.insert(
                    pane.to_string(),
                    Row {
                        status: status.to_string(),
                        ws: a
                            .get("workspace_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        agent: a
                            .get("agent")
                            .and_then(|x| x.as_str())
                            .unwrap_or("agent")
                            .to_string(),
                    },
                );
            }
        }
    }
    m
}

fn focused_ws(snap: &Value) -> String {
    snap.get("focused_workspace_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn ws_label(snap: &Value, ws: &str) -> String {
    if let Some(list) = snap.get("workspaces").and_then(|x| x.as_array()) {
        for w in list {
            if w.get("workspace_id").and_then(|x| x.as_str()) == Some(ws) {
                return w
                    .get("label")
                    .and_then(|x| x.as_str())
                    .unwrap_or(ws)
                    .to_string();
            }
        }
    }
    ws.to_string()
}

/// The batch index of a failed subscription is encoded in the error id as
/// `<request-id>:sub:<index>:probe`.
fn failed_index(id: &str) -> Option<usize> {
    let rest = id.strip_suffix(":probe")?;
    rest.rsplit(':').next()?.parse().ok()
}

/// Subscribe on a fresh connection: lifecycle events plus one
/// `pane.agent_status_changed` per known agent pane, all in ONE batch (a
/// second `events.subscribe` would close the stream).
///
/// Returns the live stream and the panes actually subscribed. Panes the server
/// rejects as `pane_not_found` (closed between the snapshot and here) are
/// dropped and the batch retried, so one stale pane can't leave us with no
/// subscription at all.
fn open_stream(mut panes: Vec<String>) -> Result<(UnixStream, Vec<String>), String> {
    for _ in 0..panes.len() + 1 {
        let mut subs: Vec<Value> = LIFECYCLE.iter().map(|t| json!({"type": t})).collect();
        for p in &panes {
            subs.push(json!({"type": "pane.agent_status_changed", "pane_id": p}));
        }

        let mut s = UnixStream::connect(sock_path()).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let req = json!({"id":"sub","method":"events.subscribe","params":{"subscriptions": subs}});
        s.write_all(format!("{}\n", req).as_bytes())
            .map_err(|e| e.to_string())?;
        s.flush().map_err(|e| e.to_string())?;

        let mut reader = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("server closed connection during subscribe".into());
        }
        let v: Value = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;

        if v.pointer("/result/type").and_then(|x| x.as_str()) == Some("subscription_started") {
            // Blocking reads from here on; the stream is one-way now.
            s.set_read_timeout(None).map_err(|e| e.to_string())?;
            return Ok((s, panes));
        }

        let code = v
            .pointer("/error/code")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if code != "pane_not_found" {
            return Err(format!("subscribe rejected: {}", line.trim()));
        }
        // Drop the pane the server rejected and retry on a fresh connection
        // (the error already closed this one).
        let idx = v
            .get("id")
            .and_then(|x| x.as_str())
            .and_then(failed_index)
            .and_then(|i| i.checked_sub(LIFECYCLE.len()));
        match idx {
            Some(i) if i < panes.len() => {
                log(&format!("dropping vanished pane {} from subscription", panes[i]));
                panes.remove(i);
            }
            _ => return Err(format!("unattributable subscribe error: {}", line.trim())),
        }
    }
    Err("could not subscribe to any pane".into())
}

fn notify(pane: &str, status: &str, agent: &str, ws_label: &str) {
    let (emoji, verb) = match status {
        "blocked" => ("⛔", "needs you"),
        "done" => ("✅", "finished"),
        _ => ("🔔", "changed"),
    };
    let sound = match status {
        "blocked" => "Sosumi",
        "done" => "Glass",
        _ => "default",
    };
    let title = format!("{} {} {}", emoji, agent, verb);
    // Clicking a toast can no longer jump anywhere: under 0.9 only a herdr
    // client keybinding can move the view (see the module docs), so there is
    // no `-open herdrfocus://` URL to attach. The hotkey is the way in.
    let message = format!("→ {}  ({} to jump)", pane, HOTKEY_HINT);
    let group = format!("herdr-notify-{}", pane);
    let _ = Command::new(TERMINAL_NOTIFIER)
        .args([
            "-title", &title,
            "-subtitle", ws_label,
            "-message", &message,
            "-group", &group,
            "-sound", sound,
        ])
        .output();
    log(&format!(
        "notified {} status={} agent={} ws='{}'",
        pane, status, agent, ws_label
    ));
}

/// An attention transition we know about: toast it if its workspace isn't the
/// one on screen.
fn raise(pane: &str, row: &Row, snap: &Value, focused: &str) {
    if row.ws != focused {
        notify(pane, &row.status, &row.agent, &ws_label(snap, &row.ws));
    }
}

/// Diff a fresh snapshot against the statuses we carried in, and fire for any
/// attention transition the event stream never delivered. 0.9 subscriptions
/// start from live events with no history replay, so this is what closes the
/// gap around a reconnect. With `seed` set we only adopt the statuses (used on
/// first start, so a session full of already-blocked agents stays quiet).
///
/// Returns the current agent pane set.
fn reconcile(snap: &Value, state: &mut State, seed: bool) -> HashSet<String> {
    let rows = agent_rows(snap);
    let focused = focused_ws(snap);
    for (pane, row) in &rows {
        let was = state.prev.get(pane).cloned().unwrap_or_default();
        let entered =
            ATTENTION.contains(&row.status.as_str()) && !ATTENTION.contains(&was.as_str());
        state.prev.insert(pane.clone(), row.status.clone());
        if seed || !entered {
            continue;
        }
        log(&format!(
            "recovered missed transition {} {} -> {}",
            pane, was, row.status
        ));
        raise(pane, row, snap, &focused);
    }
    state.prev.retain(|p, _| rows.contains_key(p));
    rows.into_keys().collect()
}

enum Msg {
    /// `pane.agent_status_changed` payload.
    Status(Value),
    /// A pane appeared or gained an agent — it may need a status subscription,
    /// which needs a new connection.
    PaneAdded,
    /// A pane went away; forget it so it can't poison the next subscribe.
    PaneGone(String),
    Disconnected,
}

/// A clean, intentional reconnect (to extend the subscription to new panes),
/// as opposed to an error.
struct Resubscribe;

/// One connect + subscribe + event loop.
///
/// `state` is carried across calls on purpose: its `prev` is the pre-gap
/// status that `reconcile` diffs the post-subscribe snapshot against, which is
/// how transitions during a reconnect still get notified.
fn run(state: &mut State) -> Result<Resubscribe, String> {
    // Discover the pane set to subscribe to. This snapshot is only used for
    // pane ids — the baseline statuses come from the one taken after the
    // subscription is live.
    let discovery = snapshot().ok_or("initial snapshot failed")?;
    let panes: Vec<String> = agent_rows(&discovery).into_keys().collect();

    // Subscribe BEFORE taking the baseline snapshot (0.9 requirement).
    let (stream, subscribed) = open_stream(panes)?;
    let mut subscribed: HashSet<String> = subscribed.into_iter().collect();

    // Baseline: everything from here on arrives as an event, and anything that
    // changed before the subscription went live is recovered by this diff.
    let base = snapshot().ok_or("baseline snapshot failed")?;
    let live = reconcile(&base, state, !state.seeded);
    if !state.seeded {
        log(&format!(
            "seeded {} agent panes (no toasts on first sync)",
            live.len()
        ));
        state.seeded = true;
    }
    log(&format!("subscribed to {} agent panes", subscribed.len()));

    // A pane created between the discovery snapshot and the subscribe has no
    // subscription; go round again rather than run half-blind.
    if live.difference(&subscribed).next().is_some() {
        return Ok(Resubscribe);
    }

    let reader_stream = stream.try_clone().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel::<Msg>();
    thread::spawn(move || {
        let mut r = BufReader::new(reader_stream);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(Msg::Disconnected);
                    break;
                }
                Ok(_) => {
                    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    // Probe events use dotted kinds, lifecycle events use
                    // snake_case ones. Both arrive on this one stream.
                    let sent = match v.get("event").and_then(|e| e.as_str()) {
                        Some("pane.agent_status_changed") => {
                            v.get("data").map(|d| tx.send(Msg::Status(d.clone())))
                        }
                        Some("pane_created") | Some("pane_agent_detected") => {
                            Some(tx.send(Msg::PaneAdded))
                        }
                        Some("pane_closed") | Some("pane_exited") => v
                            .pointer("/data/pane_id")
                            .and_then(|x| x.as_str())
                            .map(|p| tx.send(Msg::PaneGone(p.to_string()))),
                        // subscription acks and other events: ignore
                        _ => None,
                    };
                    if let Some(Err(_)) = sent {
                        break; // main loop is gone
                    }
                }
                Err(_) => {
                    let _ = tx.send(Msg::Disconnected);
                    break;
                }
            }
        }
    });

    loop {
        match rx.recv_timeout(RESYNC) {
            Ok(Msg::Status(data)) => {
                let pane = data.get("pane_id").and_then(|x| x.as_str()).unwrap_or("");
                if pane.is_empty() {
                    continue;
                }
                let row = Row {
                    status: data
                        .get("agent_status")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    ws: data
                        .get("workspace_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    agent: data
                        .get("agent")
                        .and_then(|x| x.as_str())
                        .unwrap_or("agent")
                        .to_string(),
                };
                let was = state.prev.get(pane).cloned().unwrap_or_default();
                state.prev.insert(pane.to_string(), row.status.clone());

                let entered =
                    ATTENTION.contains(&row.status.as_str()) && !ATTENTION.contains(&was.as_str());
                if !entered {
                    continue;
                }
                // Fresh snapshot to decide background (focus isn't on this
                // stream) — on its own connection, since a request on the
                // event stream would close it.
                if let Some(s) = snapshot() {
                    let focused = focused_ws(&s);
                    raise(pane, &row, &s, &focused);
                }
            }
            Ok(Msg::PaneAdded) => {
                // Only reconnect if a pane we aren't watching really has an
                // agent — plain shell panes fire pane_created too.
                if let Some(s) = snapshot() {
                    let live = reconcile(&s, state, false);
                    if let Some(new) = live.difference(&subscribed).next() {
                        log(&format!("new agent pane {}; resubscribing", new));
                        return Ok(Resubscribe);
                    }
                }
            }
            Ok(Msg::PaneGone(pane)) => {
                state.prev.remove(&pane);
                subscribed.remove(&pane);
            }
            Ok(Msg::Disconnected) => return Err("event stream disconnected".into()),
            Err(RecvTimeoutError::Timeout) => {
                // Safety net: recover anything the stream dropped, and notice
                // panes whose creation event we somehow missed.
                if let Some(s) = snapshot() {
                    let live = reconcile(&s, state, false);
                    if let Some(new) = live.difference(&subscribed).next() {
                        log(&format!("resync found unwatched agent pane {}", new));
                        return Ok(Resubscribe);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err("reader channel closed".into()),
        }
    }
}

fn main() {
    log("bridge started (rust, event-driven, herdr 0.9 protocol)");
    // Carried across reconnects so `reconcile` can spot transitions that
    // happened while we had no subscription, and so the hotkey queue survives
    // a resubscribe.
    let mut state = State::default();
    loop {
        match run(&mut state) {
            // Planned reconnect to widen the subscription: no backoff.
            Ok(Resubscribe) => {}
            Err(e) => {
                log(&format!("error: {}; reconnecting in 3s", e));
                thread::sleep(Duration::from_secs(3));
            }
        }
    }
}
