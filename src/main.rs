//! herdr system notify bridge (event-driven).
//!
//! Connects to the herdr socket, subscribes to `pane.agent_status_changed`
//! events for every agent pane, and — when a pane enters `blocked`/`done`
//! while its workspace is NOT focused — fires a macOS notification whose click
//! focuses that exact pane via the `herdrfocus://` URL handler (HerdrFocus.app).
//!
//! herdr can't do click-to-pane itself on modern macOS: its notifications use
//! `terminal-notifier -activate` (app only) and `-execute` is dead on Tahoe.
//! `-open <url>` still fires on click, so we route clicks through
//! `herdrfocus://focus/<pane>` -> `herdr agent focus <pane>`.
//!
//! Protocol (verified against the live socket): newline-delimited JSON.
//!   request : {"id","method","params"}   (params required, even for ping)
//!   snapshot: method "session.snapshot" -> result.snapshot
//!   subscribe: method "events.subscribe",
//!              params.subscriptions=[{type:"pane.agent_status_changed",pane_id}]
//!              (pane_id is REQUIRED — one subscription per pane)
//!   event   : {"event":"pane.agent_status_changed",
//!              "data":{agent,agent_status,pane_id,workspace_id}}

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

const TERMINAL_NOTIFIER: &str = "/opt/homebrew/bin/terminal-notifier";
const ATTENTION: [&str; 2] = ["blocked", "done"];
/// How often to re-scan for newly-created agent panes (there is no subscribable
/// pane-created event, so new panes are picked up by periodic rediscovery).
const REDISCOVER: Duration = Duration::from_secs(15);

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

fn agent_panes(snap: &Value) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Some(agents) = snap.get("agents").and_then(|a| a.as_array()) {
        for a in agents {
            if let (Some(pane), Some(status)) = (
                a.get("pane_id").and_then(|x| x.as_str()),
                a.get("agent_status").and_then(|x| x.as_str()),
            ) {
                m.insert(pane.to_string(), status.to_string());
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

fn subscribe(writer: &mut UnixStream, panes: &[String]) -> std::io::Result<()> {
    if panes.is_empty() {
        return Ok(());
    }
    let subs: Vec<Value> = panes
        .iter()
        .map(|p| json!({"type": "pane.agent_status_changed", "pane_id": p}))
        .collect();
    let req = json!({"id":"sub","method":"events.subscribe","params":{"subscriptions": subs}});
    writer.write_all(format!("{}\n", req).as_bytes())?;
    writer.flush()
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
    let message = format!("→ {}  (click to jump)", pane);
    let url = format!("herdrfocus://focus/{}", pane);
    let group = format!("herdr-notify-{}", pane);
    let _ = Command::new(TERMINAL_NOTIFIER)
        .args([
            "-title", &title,
            "-subtitle", ws_label,
            "-message", &message,
            "-group", &group,
            "-sound", sound,
            "-open", &url,
        ])
        .output();
    log(&format!(
        "notified {} status={} agent={} ws='{}'",
        pane, status, agent, ws_label
    ));
}

enum Msg {
    Event(Value),
    Disconnected,
}

/// One connect + subscribe + event loop. Returns Err on disconnect so `main`
/// can reconnect.
fn run() -> std::io::Result<()> {
    let snap = snapshot()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "initial snapshot failed"))?;
    let mut prev = agent_panes(&snap);
    let mut subscribed: HashSet<String> = HashSet::new();

    let stream = UnixStream::connect(sock_path())?;
    let reader_stream = stream.try_clone()?;
    let mut writer = stream;

    // Reader thread: blocking line reads -> channel. No timeout, so no risk of
    // splitting a line; the recv_timeout in the main loop drives rediscovery.
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
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                        if v.get("event").and_then(|e| e.as_str())
                            == Some("pane.agent_status_changed")
                        {
                            if let Some(data) = v.get("data") {
                                let _ = tx.send(Msg::Event(data.clone()));
                            }
                        }
                        // subscription acks / other messages: ignore
                    }
                }
                Err(_) => {
                    let _ = tx.send(Msg::Disconnected);
                    break;
                }
            }
        }
    });

    let panes: Vec<String> = prev.keys().cloned().collect();
    subscribe(&mut writer, &panes)?;
    for p in panes {
        subscribed.insert(p);
    }
    log(&format!("subscribed to {} agent panes", subscribed.len()));

    loop {
        match rx.recv_timeout(REDISCOVER) {
            Ok(Msg::Event(data)) => {
                let pane = data.get("pane_id").and_then(|x| x.as_str()).unwrap_or("");
                if pane.is_empty() {
                    continue;
                }
                let status = data
                    .get("agent_status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let ws = data
                    .get("workspace_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let agent = data
                    .get("agent")
                    .and_then(|x| x.as_str())
                    .unwrap_or("agent")
                    .to_string();
                let was = prev.get(pane).cloned().unwrap_or_default();
                prev.insert(pane.to_string(), status.clone());

                let entered =
                    ATTENTION.contains(&status.as_str()) && !ATTENTION.contains(&was.as_str());
                if !entered {
                    continue;
                }
                // Fresh snapshot to decide background (no subscribable focus event).
                if let Some(s) = snapshot() {
                    if ws != focused_ws(&s) {
                        notify(pane, &status, &agent, &ws_label(&s, &ws));
                    }
                }
            }
            Ok(Msg::Disconnected) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "event stream disconnected",
                ));
            }
            Err(RecvTimeoutError::Timeout) => {
                // Pick up newly-created agent panes.
                if let Some(s) = snapshot() {
                    let cur = agent_panes(&s);
                    let fresh: Vec<String> = cur
                        .keys()
                        .filter(|p| !subscribed.contains(*p))
                        .cloned()
                        .collect();
                    for p in &fresh {
                        prev.entry(p.clone()).or_insert_with(|| cur[p].clone());
                    }
                    if !fresh.is_empty() {
                        subscribe(&mut writer, &fresh)?;
                        for p in fresh {
                            subscribed.insert(p);
                        }
                        log(&format!("rediscovered; now {} agent panes", subscribed.len()));
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "reader channel closed",
                ));
            }
        }
    }
}

fn main() {
    log("bridge started (rust, event-driven)");
    loop {
        if let Err(e) = run() {
            log(&format!("error: {}; reconnecting in 3s", e));
        }
        thread::sleep(Duration::from_secs(3));
    }
}
