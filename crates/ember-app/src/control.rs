//! Debug control surface. When `EMBER_CONTROL=<unix socket path>` is set,
//! `ember-term` listens for line-delimited JSON commands so an agent or CI can
//! **drive and introspect the live app** without a human at the keyboard:
//! inject typed text, press named keys, run multiplexer chords, and dump grid
//! state as JSON (dims / cursor / styles / screen text).
//!
//! Wire protocol (one JSON object per line, one request per connection):
//!   {"cmd":"type","text":"ls\n"}      -> {"ok":true}
//!   {"cmd":"key","name":"Enter"}      -> {"ok":true}
//!   {"cmd":"chord","keys":"cmd+d"}    -> {"ok":true}
//!   {"cmd":"state"}                   -> {"ok":true,"state":{...}}
//!
//! The listener runs on a background thread and forwards [`ControlMsg`]s to the
//! winit event loop, which drains them each poll tick (see `about_to_wait`). A
//! `state` request round-trips through a reply channel so the main thread can
//! build the snapshot. The matching client lives in [`client`] (`ember-term ctl`).

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
}

#[cfg(unix)]
pub use unix::{client, spawn_listener};

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    use super::ControlMsg;

    /// Bind the control socket and spawn the accept loop. Returns the receiver the
    /// event loop drains. Each connection carries one request line and gets one
    /// response line.
    pub fn spawn_listener(path: &str) -> std::io::Result<Receiver<ControlMsg>> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let tx = tx.clone();
                // Handle each connection inline; requests are tiny and infrequent.
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
            return Ok(());
        }
        let resp = dispatch(&line, tx);
        writeln!(stream, "{resp}")
    }

    /// Parse one request line and forward it; returns the JSON response line.
    fn dispatch(line: &str, tx: &Sender<ControlMsg>) -> String {
        let v: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => return format!("{{\"ok\":false,\"error\":\"bad json: {e}\"}}"),
        };
        let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
        match cmd {
            "type" => {
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                let _ = tx.send(ControlMsg::Type(text.to_string()));
                ok()
            }
            "key" => {
                let name = v.get("name").and_then(|t| t.as_str()).unwrap_or_default();
                let _ = tx.send(ControlMsg::Key(name.to_string()));
                ok()
            }
            "chord" => {
                let keys = v.get("keys").and_then(|t| t.as_str()).unwrap_or_default();
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
            other => err(&format!("unknown cmd: {other}")),
        }
    }

    fn ok() -> String {
        "{\"ok\":true}".to_string()
    }
    fn err(msg: &str) -> String {
        format!("{{\"ok\":false,\"error\":\"{msg}\"}}")
    }

    /// `ember-term ctl <cmd> [arg]` — the client side. Connects to the socket
    /// (`--sock`, else `$EMBER_CONTROL`, else `/tmp/ember.sock`), sends one request,
    /// and prints the response. `type` unescapes `\n`/`\t`/`\\` in its argument.
    pub fn client(args: &[String]) -> Result<(), String> {
        // args = ["ctl", <cmd>, <arg?>...] plus optional `--sock <path>`.
        let mut sock = std::env::var("EMBER_CONTROL").unwrap_or_else(|_| "/tmp/ember.sock".into());
        let mut rest: Vec<&String> = Vec::new();
        let mut it = args.iter().skip(1); // skip "ctl"
        while let Some(a) = it.next() {
            if a == "--sock" {
                sock = it.next().ok_or("--sock needs a path")?.clone();
            } else {
                rest.push(a);
            }
        }
        let cmd = rest.first().map(|s| s.as_str()).unwrap_or("state");
        let arg = rest.get(1).map(|s| s.as_str()).unwrap_or("");
        let request = match cmd {
            "type" => serde_json::json!({"cmd":"type","text": unescape(arg)}),
            "key" => serde_json::json!({"cmd":"key","name": arg}),
            "chord" => serde_json::json!({"cmd":"chord","keys": arg}),
            "state" => serde_json::json!({"cmd":"state"}),
            other => return Err(format!("unknown ctl cmd: {other} (type|key|chord|state)")),
        };

        let mut stream = UnixStream::connect(&sock).map_err(|e| {
            format!("connect {sock}: {e} (is ember-term running with EMBER_CONTROL={sock}?)")
        })?;
        writeln!(stream, "{request}").map_err(|e| format!("write: {e}"))?;
        let mut reader = BufReader::new(stream);
        let mut resp = String::new();
        reader
            .read_line(&mut resp)
            .map_err(|e| format!("read: {e}"))?;
        print!("{resp}");
        Ok(())
    }

    /// Turn `\n` / `\t` / `\\` escapes in a CLI argument into real characters, so
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
pub fn spawn_listener(_path: &str) -> std::io::Result<std::sync::mpsc::Receiver<ControlMsg>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "control socket is unix-only",
    ))
}

#[cfg(not(unix))]
pub fn client(_args: &[String]) -> Result<(), String> {
    Err("ember-term ctl is unix-only".to_string())
}
