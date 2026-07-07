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

use ember_core::ScrollAmount;

/// `ctl move-tab <arg>`'s destination: a brand-new window, an existing
/// 1-based window number, or the window adjacent to the focused one in
/// `Shared::window_order`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTabTarget {
    New,
    Window(usize),
    Next,
    Prev,
}

/// `ctl promote-pane <arg>`'s destination: the pane's own new tab (same
/// window) or a brand-new window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromotePaneTarget {
    Tab,
    Window,
}

/// A command forwarded from the control socket to the event loop.
pub enum ControlMsg {
    /// Type raw text into the focused session (newlines included).
    Type(String),
    /// Press a named key (Enter/Tab/Escape/Backspace/Space/Arrow*) in the session.
    Key(String),
    /// Run a chord like `cmd+d`, `cmd+shift+arrowright`, `cmd+1`.
    Chord(String),
    /// Request a JSON state dump; the main thread replies on the channel.
    /// The reply's top-level `windows` array covers every open window
    /// (`{id,focused,active_tab,tabs}`, in `Shared::window_order`); the
    /// existing top-level fields (`tabs`/`active_tab`/`panes`/etc.) still
    /// describe the FOCUSED window, for callers that predate multi-window.
    State(Sender<String>),
    /// Focus the first tab, searched across EVERY window (window order, then
    /// tab order within each) whose displayed title contains the query
    /// (case-insensitive), then raise that window. Reply is the full JSON
    /// response line: `{"ok":true,"index":..,"title":..,"window":..}` (both
    /// 1-based) or a not-found error that lists every window's titles seen,
    /// flattened in search order.
    Focus(String, Sender<String>),
    /// Bring the window to the front and give it keyboard focus.
    Raise,
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
    /// Toggle the FPS/frame-time debug overlay.
    Fps,
    /// Inject a BEL for a tab's session (visual bell); `None` = the focused pane.
    Bell(Option<usize>),
    /// Move the tab at index `from` to index `to` (drag-reorder, for tests).
    ReorderTab(usize, usize),
    /// Set tab `i`'s title to the given name (rename, for tests).
    RenameTab(usize, String),
    /// Begin inline rename of tab `i` (to screenshot the edit caret).
    EditTab(usize),
    /// Scroll the focused pane's scrollback (for tests / accessibility).
    Scroll(ScrollAmount),
    /// Open a new OS window with one fresh tab (mirrors Cmd+N / the File →
    /// New Window menu item), cwd inherited from the focused pane.
    NewWindow,
    /// Move the focused tab to `target` (a brand-new window, an existing
    /// 1-based window number, or the next/previous window). Replies with the
    /// full JSON response line (`{"ok":true}` / `{"ok":false,"error":..}`) —
    /// handled at the `App` level (needs the live window set + event loop),
    /// same reasoning as `NewWindow`.
    MoveTab(MoveTabTarget, Sender<String>),
    /// Promote the focused pane to its own tab (same window) or its own
    /// brand-new window. Same reply contract as `MoveTab`.
    PromotePane(PromotePaneTarget, Sender<String>),
    /// Merge the focused tab into the tab immediately before it, as a
    /// horizontal split of that tab's focused pane. Same reply contract as
    /// `MoveTab`; errors (e.g. no previous tab) surface in the reply.
    MergeTab(Sender<String>),
    /// Synthesize a full drag gesture on the focused window: a left press at
    /// `(x1, y1)`, `steps` intermediate motions on the way to `(x2, y2)`,
    /// then either a release at `(x2, y2)` or (if `cancel`) an Escape — all
    /// through the exact same handlers a real mouse/keyboard hits. `mods` is
    /// a `parse_chord`-style `+`-joined modifier list (e.g. `"cmd+alt"`,
    /// possibly empty) held for the whole gesture. The testing backbone for
    /// every surface-drag task: replies with a JSON summary
    /// (`{"ok":true,"drag_ended":"reorder"|"move"|"cancel"|"selection"|"none","drag_active_mid":bool}`).
    Drag {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        steps: usize,
        mods: String,
        cancel: bool,
        reply: Sender<String>,
    },
}

#[cfg(unix)]
pub use unix::{
    ControlServer, client, list_instances, resolve_socket, send, server_bind_path, spawn_listener,
};

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    use ember_core::ScrollAmount;
    use serde_json::Value;

    use super::{ControlMsg, MoveTabTarget, PromotePaneTarget};

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
    ///
    /// The socket accepts keystroke injection and full screen-text reads, so it
    /// must be reachable by this user only: the dir is created 0700 (and must
    /// be OURS — on shared /tmp another user could pre-squat the fixed name),
    /// and the socket itself is chmod 0600.
    /// A running control listener you can stop (unbinds + removes the socket).
    pub struct ControlServer {
        bind_path: PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl ControlServer {
        /// The socket path clients connect to.
        pub fn path(&self) -> &Path {
            &self.bind_path
        }

        /// Stop accepting, then remove the socket. Unblocks the accept loop with
        /// a throwaway self-connection.
        pub fn stop(self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = UnixStream::connect(&self.bind_path);
            let _ = std::fs::remove_file(&self.bind_path);
        }
    }

    pub fn spawn_listener(
        bind_path: &Path,
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<(Receiver<ControlMsg>, ControlServer)> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Some(dir) = bind_path.parent() {
            std::fs::create_dir_all(dir)?;
            let meta = std::fs::metadata(dir)?;
            if meta.uid() != process_uid() {
                return Err(std::io::Error::other(format!(
                    "control dir {} is owned by uid {} (not us) — refusing to bind",
                    dir.display(),
                    meta.uid()
                )));
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        // Only our own (per-PID) path is removed here, so we never clobber another
        // live instance's socket.
        let _ = std::fs::remove_file(bind_path);
        let listener = UnixListener::bind(bind_path)?;
        std::fs::set_permissions(bind_path, std::fs::Permissions::from_mode(0o600))?;
        let (tx, rx) = mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(std::sync::atomic::Ordering::Relaxed) {
                    break; // stop() self-connected to unblock us
                }
                let Ok(stream) = stream else { continue };
                if let Err(e) = serve(stream, &tx, &*waker) {
                    eprintln!("[ember-control] request error: {e}");
                }
                // The event loop sleeps on ControlFlow::Wait; wake it so the
                // just-forwarded command is drained this cycle, not on the next
                // unrelated event. (Reply-waiting commands also wake it INSIDE
                // dispatch — this after-the-fact wake alone stranded them: on a
                // fully quiet window — hours idle, occluded, locked display —
                // nothing else wakes the loop within the reply timeout, which
                // was the daily-driver "state timeout" failure.)
                waker();
            }
        });
        Ok((
            rx,
            ControlServer {
                bind_path: bind_path.to_path_buf(),
                stop,
            },
        ))
    }

    fn serve(
        mut stream: UnixStream,
        tx: &Sender<ControlMsg>,
        waker: &(dyn Fn() + Send + Sync),
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // liveness probe (no payload) — just close.
        }
        let resp = dispatch(&line, tx, waker);
        writeln!(stream, "{resp}")
    }

    /// Parse one request line and forward it; returns the JSON response line.
    ///
    /// `waker` rouses the `ControlFlow::Wait`-parked event loop. Commands that
    /// wait for a reply MUST call it right after their `tx.send` — the loop only
    /// drains `control_rx` in `about_to_wait`, so without an immediate wake the
    /// reply wait races whatever unrelated event happens to arrive next (none
    /// ever does on an idle occluded window → guaranteed timeout).
    fn dispatch(line: &str, tx: &Sender<ControlMsg>, waker: &(dyn Fn() + Send + Sync)) -> String {
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
                waker(); // wake BEFORE waiting — see dispatch docs
                match reply_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(state) => format!("{{\"ok\":true,\"state\":{state}}}"),
                    Err(_) => err("state timeout"),
                }
            }
            "screenshot" => {
                let path = v
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    // Default inside the owner-only control dir — a fixed
                    // world-writable /tmp name is a symlink-clobber target.
                    .unwrap_or_else(|| {
                        socket_dir()
                            .join(format!("{}-live.png", std::process::id()))
                            .to_string_lossy()
                            .into_owned()
                    });
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlMsg::Screenshot(path, reply_tx)).is_err() {
                    return err("event loop gone");
                }
                waker(); // wake BEFORE waiting — see dispatch docs
                match reply_rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(resp) => resp, // main builds the full JSON response.
                    Err(_) => err("screenshot timeout"),
                }
            }
            "focus" => {
                let Some(query) = v.get("query").and_then(Value::as_str) else {
                    return err("focus needs a query");
                };
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx
                    .send(ControlMsg::Focus(query.to_string(), reply_tx))
                    .is_err()
                {
                    return err("event loop gone");
                }
                waker(); // wake BEFORE waiting — see dispatch docs
                match reply_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(resp) => resp, // main builds the full JSON response.
                    Err(_) => err("focus timeout"),
                }
            }
            "raise" => {
                let _ = tx.send(ControlMsg::Raise);
                ok()
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
            "new-window" => {
                let _ = tx.send(ControlMsg::NewWindow);
                ok()
            }
            "move-tab" => {
                let arg = v.get("to").and_then(Value::as_str).unwrap_or("");
                let target = match arg {
                    "new" => MoveTabTarget::New,
                    "next" => MoveTabTarget::Next,
                    "prev" => MoveTabTarget::Prev,
                    n => match n.parse::<usize>() {
                        Ok(w) if w >= 1 => MoveTabTarget::Window(w),
                        _ => return err("move-tab: to = new|next|prev|<1-based window number>"),
                    },
                };
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlMsg::MoveTab(target, reply_tx)).is_err() {
                    return err("event loop gone");
                }
                waker(); // wake BEFORE waiting — see dispatch docs
                match reply_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(resp) => resp,
                    Err(_) => err("move-tab timeout"),
                }
            }
            "promote-pane" => {
                let arg = v.get("to").and_then(Value::as_str).unwrap_or("");
                let target = match arg {
                    "tab" => PromotePaneTarget::Tab,
                    "window" => PromotePaneTarget::Window,
                    _ => return err("promote-pane: to = tab|window"),
                };
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlMsg::PromotePane(target, reply_tx)).is_err() {
                    return err("event loop gone");
                }
                waker(); // wake BEFORE waiting — see dispatch docs
                match reply_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(resp) => resp,
                    Err(_) => err("promote-pane timeout"),
                }
            }
            "merge-tab" => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(ControlMsg::MergeTab(reply_tx)).is_err() {
                    return err("event loop gone");
                }
                waker(); // wake BEFORE waiting — see dispatch docs
                match reply_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(resp) => resp,
                    Err(_) => err("merge-tab timeout"),
                }
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
            "fps" => {
                let _ = tx.send(ControlMsg::Fps);
                ok()
            }
            "scroll" => {
                let dir = v.get("dir").and_then(Value::as_str).unwrap_or("");
                let amt = match dir {
                    "top" => ScrollAmount::Top,
                    "bottom" => ScrollAmount::Bottom,
                    "page-up" | "pageup" => ScrollAmount::PageUp,
                    "page-down" | "pagedown" => ScrollAmount::PageDown,
                    n => match n.parse::<i32>() {
                        Ok(n) => ScrollAmount::Lines(n),
                        Err(_) => return err("scroll: dir = top|bottom|page-up|page-down|<lines>"),
                    },
                };
                let _ = tx.send(ControlMsg::Scroll(amt));
                ok()
            }
            "bell" => {
                let tab = v.get("tab").and_then(Value::as_u64).map(|n| n as usize);
                let _ = tx.send(ControlMsg::Bell(tab));
                ok()
            }
            "reorder-tab" => {
                let g = |k| v.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
                let _ = tx.send(ControlMsg::ReorderTab(g("from"), g("to")));
                ok()
            }
            "rename-tab" => {
                let i = v.get("i").and_then(Value::as_u64).unwrap_or(0) as usize;
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let _ = tx.send(ControlMsg::RenameTab(i, name.to_string()));
                ok()
            }
            "edit-tab" => {
                let i = v.get("i").and_then(Value::as_u64).unwrap_or(0) as usize;
                let _ = tx.send(ControlMsg::EditTab(i));
                ok()
            }
            "drag" => {
                let g = |k| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                let steps = v.get("steps").and_then(Value::as_u64).unwrap_or(8) as usize;
                let mods = v
                    .get("mods")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let cancel = v.get("cancel").and_then(Value::as_bool).unwrap_or(false);
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx
                    .send(ControlMsg::Drag {
                        x1: g("x1"),
                        y1: g("y1"),
                        x2: g("x2"),
                        y2: g("y2"),
                        steps,
                        mods,
                        cancel,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return err("event loop gone");
                }
                waker(); // wake BEFORE waiting — see dispatch docs
                match reply_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(resp) => resp,
                    Err(_) => err("drag timeout"),
                }
            }
            other => err(&format!("unknown cmd: {other}")),
        }
    }

    fn ok() -> String {
        "{\"ok\":true}".to_string()
    }
    /// Proper JSON encoding — `msg` may carry client-controlled text (a typo'd
    /// cmd, a serde error quoting the input) and interpolation would emit an
    /// unparsable response on any embedded quote.
    fn err(msg: &str) -> String {
        serde_json::json!({"ok": false, "error": msg}).to_string()
    }

    /// This process's uid (for ownership checks on shared-tmp paths).
    #[allow(unsafe_code)] // getuid is unconditionally safe; std exposes no wrapper
    fn process_uid() -> u32 {
        unsafe { libc::getuid() }
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
            "focus" => {
                // Join the remaining args so `ctl focus agent alpha` works
                // without quoting.
                let query = rest
                    .get(1..)
                    .map(|r| r.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                if query.is_empty() {
                    return Err("focus needs a query (matches tab titles)".to_string());
                }
                serde_json::json!({"cmd":"focus","query": query})
            }
            "raise" => serde_json::json!({"cmd":"raise"}),
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
            "new-window" => serde_json::json!({"cmd":"new-window"}),
            "move-tab" => serde_json::json!({"cmd":"move-tab","to": arg}),
            "promote-pane" => serde_json::json!({"cmd":"promote-pane","to": arg}),
            "merge-tab" => serde_json::json!({"cmd":"merge-tab"}),
            "settings" => serde_json::json!({"cmd":"settings"}),
            "select" => {
                let g = |i: usize| rest.get(i).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
                let mode = rest.get(5).map(|s| s.as_str()).unwrap_or("simple");
                serde_json::json!({"cmd":"select","r1":g(1),"c1":g(2),"r2":g(3),"c2":g(4),"mode":mode})
            }
            "copy" => serde_json::json!({"cmd":"copy"}),
            "paste" => serde_json::json!({"cmd":"paste","text": unescape(arg)}),
            "fps" => serde_json::json!({"cmd":"fps"}),
            "scroll" => serde_json::json!({"cmd":"scroll","dir": arg}),
            "bell" => match rest.get(1).and_then(|s| s.parse::<u64>().ok()) {
                Some(t) => serde_json::json!({"cmd":"bell","tab":t}),
                None => serde_json::json!({"cmd":"bell"}),
            },
            "reorder-tab" => {
                let g = |i: usize| rest.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                serde_json::json!({"cmd":"reorder-tab","from":g(1),"to":g(2)})
            }
            "rename-tab" => {
                let i = rest.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let name = rest
                    .get(2..)
                    .map(|r| r.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                serde_json::json!({"cmd":"rename-tab","i":i,"name":name})
            }
            "edit-tab" => {
                let i = rest.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                serde_json::json!({"cmd":"edit-tab","i":i})
            }
            "drag" => {
                let owned: Vec<String> = rest
                    .get(1..)
                    .unwrap_or(&[])
                    .iter()
                    .map(|s| (*s).clone())
                    .collect();
                parse_drag_args(&owned)?
            }
            other => {
                return Err(format!(
                    "unknown ctl cmd: {other} (list|type|key|chord|state|focus|raise|screenshot|click|about|settings|select|copy|paste|reorder-tab|rename-tab|edit-tab|new-window|move-tab|promote-pane|merge-tab|drag)"
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

    /// Parse `ctl drag`'s args (everything after the `drag` token itself)
    /// into the JSON request line: 4 positional logical-px coordinates
    /// (`x1 y1 x2 y2`, in any order relative to the `--steps`/`--mods`/
    /// `--cancel` flags) plus the optional flags. A pure function (no
    /// socket I/O) so the parsing itself is unit-testable independent of a
    /// running instance.
    fn parse_drag_args(args: &[String]) -> Result<Value, String> {
        let mut positional: Vec<f64> = Vec::new();
        let mut steps: u64 = 8;
        let mut mods = String::new();
        let mut cancel = false;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--steps" => {
                    let n = it.next().ok_or("drag: --steps needs a number")?;
                    steps = n
                        .parse()
                        .map_err(|e| format!("drag: --steps: bad number {n:?}: {e}"))?;
                }
                "--mods" => {
                    mods = it.next().ok_or("drag: --mods needs a value")?.clone();
                }
                "--cancel" => cancel = true,
                v => positional.push(
                    v.parse()
                        .map_err(|e| format!("drag: bad coordinate {v:?}: {e}"))?,
                ),
            }
        }
        if positional.len() != 4 {
            return Err(format!(
                "drag: need x1 y1 x2 y2 [--steps N] [--mods m] [--cancel] (got {} coordinate(s))",
                positional.len()
            ));
        }
        Ok(serde_json::json!({
            "cmd": "drag",
            "x1": positional[0],
            "y1": positional[1],
            "x2": positional[2],
            "y2": positional[3],
            "steps": steps,
            "mods": mods,
            "cancel": cancel,
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::parse_drag_args;

        fn args(s: &str) -> Vec<String> {
            s.split_whitespace().map(str::to_string).collect()
        }

        #[test]
        fn drag_parses_four_coordinates_with_defaults() {
            let v = parse_drag_args(&args("10 20 30 40")).unwrap();
            assert_eq!(v["cmd"], "drag");
            assert_eq!(v["x1"], 10.0);
            assert_eq!(v["y1"], 20.0);
            assert_eq!(v["x2"], 30.0);
            assert_eq!(v["y2"], 40.0);
            assert_eq!(v["steps"], 8);
            assert_eq!(v["mods"], "");
            assert_eq!(v["cancel"], false);
        }

        #[test]
        fn drag_parses_steps_mods_cancel_in_any_position() {
            let v = parse_drag_args(&args("--mods cmd+alt 1 2 3 4 --steps 3 --cancel")).unwrap();
            assert_eq!(v["x1"], 1.0);
            assert_eq!(v["y2"], 4.0);
            assert_eq!(v["steps"], 3);
            assert_eq!(v["mods"], "cmd+alt");
            assert_eq!(v["cancel"], true);
        }

        #[test]
        fn drag_rejects_wrong_coordinate_count() {
            assert!(parse_drag_args(&args("1 2 3")).is_err());
            assert!(parse_drag_args(&args("1 2 3 4 5")).is_err());
            assert!(parse_drag_args(&args("")).is_err());
        }

        #[test]
        fn drag_rejects_bad_coordinate() {
            assert!(parse_drag_args(&args("x 2 3 4")).is_err());
        }

        #[test]
        fn drag_rejects_dangling_flags() {
            assert!(parse_drag_args(&args("1 2 3 4 --steps")).is_err());
            assert!(parse_drag_args(&args("1 2 3 4 --mods")).is_err());
        }
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
