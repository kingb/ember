//! Debug control surface. When `EMBER_CONTROL` is set, `ember-term`
//! listens on a unix socket for line-delimited JSON commands, so an agent or CI
//! can **drive and introspect the live app** without a human at the keyboard:
//! inject typed text, press named keys, run multiplexer chords, and dump grid
//! state as JSON (dims / cursor / styles / screen text).
//!
//! **One socket per instance.** `EMBER_CONTROL=1` (or `auto`) binds a per-PID
//! socket under `$TMPDIR/ember-ctl/<pid>.sock` so multiple ember-terms never
//! collide; `EMBER_CONTROL=/explicit/path` uses that path verbatim (single
//! instance). The client (`ember-term ctl`) discovers instances by scanning that
//! directory: with exactly one live instance it auto-targets it, otherwise it
//! lists them and asks for `--pid`/`--sock`.
//!
//! Wire protocol (one JSON object per line, one request per connection):
//!   {"cmd":"type","text":"ls\n"}      -> {"ok":true}
//!   {"cmd":"key","name":"Enter"}      -> {"ok":true}
//!   {"cmd":"chord","keys":"cmd+d"}    -> {"ok":true}
//!   {"cmd":"state"}                   -> {"ok":true,"state":{...}}

use std::sync::mpsc::Sender;

/// A command forwarded from the control socket to the event loop.
pub enum ControlMsg {
    /// Type raw text into the focused session (newlines included).
    Type(String),
    /// Press a named key (Enter/Tab/Escape/Backspace/Space/Arrow*) in the session.
    Key(String),
    /// Run a chord like `cmd+d`, `cmd+shift+arrowright`, `cmd+1`.
    Chord(String),
    /// Request a JSON state dump; the main thread replies on the channel.
    State(Sender<String>),
    /// Capture the live window to a PNG at the given path; reply is the full
    /// JSON response line (`{"ok":true,"path":..}` / `{"ok":false,"error":..}`).
    Screenshot(String, Sender<String>),
    /// Left-click at logical `(x, y)` — for driving tabs/UI in tests.
    Click(f64, f64),
    /// Toggle the About overlay (the menu item isn't injectable in tests).
    About,
    /// Toggle the Settings overlay (the menu item isn't injectable in tests).
    Settings,
    /// Set a selection on the focused pane: `(r1, c1, r2, c2, mode)` where mode is
    /// `simple` | `word` | `line`.
    Select(u16, u16, u16, u16, String),
    /// Copy the current selection to the clipboard.
    Copy,
    /// Paste the given text into the focused pane (as if from the clipboard).
    Paste(String),
}

#[cfg(unix)]
pub use unix::{client, list_instances, resolve_socket, send, server_bind_path, spawn_listener};

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    use serde_json::Value;

    use super::ControlMsg;

    /// Directory holding per-instance sockets: `$TMPDIR/ember-ctl/`.
    pub fn socket_dir() -> PathBuf {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("ember-ctl")
    }

    fn pid_socket(pid: u32) -> PathBuf {
        socket_dir().join(format!("{pid}.sock"))
    }

    /// Resolve the path this process should bind from the `EMBER_CONTROL` value: an
    /// explicit path (contains `/`) verbatim, else a per-PID socket in the dir.
    pub fn server_bind_path(env_val: &str) -> PathBuf {
        if env_val.contains('/') {
            PathBuf::from(env_val)
        } else {
            pid_socket(std::process::id())
        }
    }

    /// Bind the control socket and spawn the accept loop. Returns the receiver the
    /// event loop drains. Each connection carries one request line + one response.
    pub fn spawn_listener(bind_path: &Path) -> std::io::Result<Receiver<ControlMsg>> {
        if let Some(dir) = bind_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Only our own (per-PID) path is removed here, so we never clobber another
        // live instance's socket.
        let _ = std::fs::remove_file(bind_path);
        let listener = UnixListener::bind(bind_path)?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if let Err(e) = serve(stream, &tx) {
                    eprintln!("[ember-control] request error: {e}");
                }
            }
        });
        Ok(rx)
    }

    fn serve(mut stream: UnixStream, tx: &Sender<ControlMsg>) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // liveness probe (no payload) — just close.
        }
        let resp = dispatch(&line, tx);
        writeln!(stream, "{resp}")
    }

    /// Parse one request line and forward it; returns the JSON response line.
    fn dispatch(line: &str, tx: &Sender<ControlMsg>) -> String {
        let v: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => return format!("{{\"ok\":false,\"error\":\"bad json: {e}\"}}"),
        };
        match v.get("cmd").and_then(Value::as_str).unwrap_or("") {
            "type" => {
                let text = v.get("text").and_then(Value::as_str).unwrap_or_default();
                let _ = tx.send(ControlMsg::Type(text.to_string()));
                ok()
            }
            "key" => {
                let name = v.get("name").and_then(Value::as_str).unwrap_or_default();
                let _ = tx.send(ControlMsg::Key(name.to_string()));
                ok()
            }
            "chord" => {
                let keys = v.get("keys").and_then(Value::as_str).unwrap_or_default();
                let _ = tx.send(ControlMsg::Chord(keys.to_string()));
                ok()
            }
            "state" => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlMsg::State(reply_tx)).is_err() {
                    return err("event loop gone");
                }
                match reply_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(state) => format!("{{\"ok\":true,\"state\":{state}}}"),
                    Err(_) => err("state timeout"),
                }
            }
            "screenshot" => {
                let path = v
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("/tmp/ember-live.png")
                    .to_string();
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlMsg::Screenshot(path, reply_tx)).is_err() {
                    return err("event loop gone");
                }
                match reply_rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(resp) => resp, // main builds the full JSON response.
                    Err(_) => err("screenshot timeout"),
                }
            }
            "click" => {
                let x = v.get("x").and_then(Value::as_f64).unwrap_or(0.0);
                let y = v.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                let _ = tx.send(ControlMsg::Click(x, y));
                ok()
            }
            "about" => {
                let _ = tx.send(ControlMsg::About);
                ok()
            }
            "settings" => {
                let _ = tx.send(ControlMsg::Settings);
                ok()
            }
            "select" => {
                let g = |k| v.get(k).and_then(Value::as_u64).unwrap_or(0) as u16;
                let mode = v
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("simple")
                    .to_string();
                let _ = tx.send(ControlMsg::Select(g("r1"), g("c1"), g("r2"), g("c2"), mode));
                ok()
            }
            "copy" => {
                let _ = tx.send(ControlMsg::Copy);
                ok()
            }
            "paste" => {
                let text = v.get("text").and_then(Value::as_str).unwrap_or("");
                let _ = tx.send(ControlMsg::Paste(text.to_string()));
                ok()
            }
            other => err(&format!("unknown cmd: {other}")),
        }
    }

    fn ok() -> String {
        "{\"ok\":true}".to_string()
    }
    fn err(msg: &str) -> String {
        format!("{{\"ok\":false,\"error\":\"{msg}\"}}")
    }

    /// Live instances as `(pid, socket_path)`, by scanning the socket dir and
    /// probing each. Stale socket files (no listener) are pruned best-effort.
    pub fn list_instances() -> Vec<(u32, PathBuf)> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(socket_dir()) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let pid = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok());
            let Some(pid) = pid else { continue };
            if UnixStream::connect(&path).is_ok() {
                out.push((pid, path));
            } else {
                let _ = std::fs::remove_file(&path); // prune stale.
            }
        }
        out.sort_by_key(|(pid, _)| *pid);
        out
    }

    /// Resolve which socket the client should talk to: explicit `--sock`, else
    /// `--pid`, else the sole live instance (error + list if zero or many).
    pub fn resolve_socket(sock: Option<String>, pid: Option<u32>) -> Result<PathBuf, String> {
        if let Some(s) = sock {
            return Ok(PathBuf::from(s));
        }
        if let Some(p) = pid {
            return Ok(pid_socket(p));
        }
        let live = list_instances();
        match live.len() {
            1 => Ok(live[0].1.clone()),
            0 => Err(
                "no running ember-term with a control socket (launch with EMBER_CONTROL=1)".into(),
            ),
            _ => {
                let pids: Vec<String> = live.iter().map(|(p, _)| p.to_string()).collect();
                Err(format!(
                    "multiple instances ({}); pass --pid <PID> or --sock <PATH>",
                    pids.join(", ")
                ))
            }
        }
    }

    /// Send one request to `socket` and return the response line.
    pub fn send(socket: &Path, request: &Value) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket)
            .map_err(|e| format!("connect {}: {e}", socket.display()))?;
        writeln!(stream, "{request}").map_err(|e| format!("write: {e}"))?;
        let mut reader = BufReader::new(stream);
        let mut resp = String::new();
        reader
            .read_line(&mut resp)
            .map_err(|e| format!("read: {e}"))?;
        Ok(resp.trim_end().to_string())
    }

    /// `ember-term ctl [--sock P | --pid N] <list|type|key|chord|state> [arg]`.
    pub fn client(args: &[String]) -> Result<(), String> {
        let mut sock: Option<String> = None;
        let mut pid: Option<u32> = None;
        let mut rest: Vec<&String> = Vec::new();
        let mut it = args.iter().skip(1); // skip "ctl"
        while let Some(a) = it.next() {
            match a.as_str() {
                "--sock" => sock = Some(it.next().ok_or("--sock needs a path")?.clone()),
                "--pid" => {
                    pid = Some(
                        it.next()
                            .ok_or("--pid needs a number")?
                            .parse()
                            .map_err(|e| format!("--pid: {e}"))?,
                    )
                }
                _ => rest.push(a),
            }
        }
        let cmd = rest.first().map(|s| s.as_str()).unwrap_or("state");
        let arg = rest.get(1).map(|s| s.as_str()).unwrap_or("");

        if cmd == "list" {
            let live = list_instances();
            if live.is_empty() {
                println!("(no running ember-term control sockets)");
            }
            for (pid, path) in live {
                println!("pid {pid}\t{}", path.display());
            }
            return Ok(());
        }

        let request = match cmd {
            "type" => serde_json::json!({"cmd":"type","text": unescape(arg)}),
            "key" => serde_json::json!({"cmd":"key","name": arg}),
            "chord" => serde_json::json!({"cmd":"chord","keys": arg}),
            "state" => serde_json::json!({"cmd":"state"}),
            "screenshot" => {
                let path = if arg.is_empty() {
                    "/tmp/ember-live.png"
                } else {
                    arg
                };
                serde_json::json!({"cmd":"screenshot","path": path})
            }
            "click" => {
                let x: f64 = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let y: f64 = rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                serde_json::json!({"cmd":"click","x": x, "y": y})
            }
            "about" => serde_json::json!({"cmd":"about"}),
            "settings" => serde_json::json!({"cmd":"settings"}),
            "select" => {
                let g = |i: usize| rest.get(i).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
                let mode = rest.get(5).map(|s| s.as_str()).unwrap_or("simple");
                serde_json::json!({"cmd":"select","r1":g(1),"c1":g(2),"r2":g(3),"c2":g(4),"mode":mode})
            }
            "copy" => serde_json::json!({"cmd":"copy"}),
            "paste" => serde_json::json!({"cmd":"paste","text": unescape(arg)}),
            other => {
                return Err(format!(
                    "unknown ctl cmd: {other} (list|type|key|chord|state|screenshot|click|about|settings|select|copy|paste)"
                ));
            }
        };
        let socket = resolve_socket(sock, pid)?;
        let resp = send(&socket, &request)?;
        println!("{resp}");
        Ok(())
    }

    /// Turn `\n` / `\t` / `\r` / `\\` escapes in a CLI argument into real chars, so
    /// `ctl type "ls\n"` actually presses Enter.
    fn unescape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}

#[cfg(not(unix))]
pub fn spawn_listener(
    _p: &std::path::Path,
) -> std::io::Result<std::sync::mpsc::Receiver<ControlMsg>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "control socket is unix-only",
    ))
}

#[cfg(not(unix))]
pub fn server_bind_path(_env_val: &str) -> std::path::PathBuf {
    std::path::PathBuf::new()
}

#[cfg(not(unix))]
pub fn client(_args: &[String]) -> Result<(), String> {
    Err("ember-term ctl is unix-only".to_string())
}
