//! Session snapshot model, atomic writer, and debounce thread.
//!
//! Lives at `$XDG_STATE_HOME/ember/session.json` (else `~/.local/state/ember/session.json`).
//! Defines its own plain serde structs, decoupled from `ember_core`'s in-memory
//! layout types, so the on-disk schema is stable independent of internal
//! refactors. `assemble` maps the live window/tab/split/pane state into that
//! schema; `main.rs`'s `session_dirty` calls it on every structural or
//! content mutation and feeds the result to `SnapshotWriter`, which
//! debounces (300ms quiet, 1s max defer) and writes it atomically so a
//! crash never leaves a corrupt or partial file on disk.
//!
//! Restore-on-launch (reading this file back at startup) is a later task —
//! this module only ever writes it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Top-level on-disk snapshot: one window list, plus a schema version so a
/// future format change can migrate or discard old files instead of failing
/// to parse.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub version: u32,
    pub saved_at: String,
    pub windows: Vec<WindowSnap>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WindowSnap {
    pub pos: Option<(i32, i32)>,
    pub size: (u32, u32),
    pub focused_tab: usize,
    pub tabs: Vec<TabSnap>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TabSnap {
    pub name: String,
    pub named_by_user: bool,
    pub splits: NodeSnap,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum NodeSnap {
    Pane(PaneSnap),
    Split {
        dir: char,
        ratio: f32,
        a: Box<NodeSnap>,
        b: Box<NodeSnap>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PaneSnap {
    pub cwd: Option<String>,
    pub last_cmd: Option<String>,
    pub was_running: bool,
}

/// Truncate `s` to at most 1024 bytes, backing off to the nearest earlier
/// char boundary so a multi-byte UTF-8 character is never split. The single
/// enforcement point for the on-disk `last_cmd` size cap — everything else
/// that stores a command string (`Shared::pane_meta`, `PaneSnap`) is
/// unbounded, and relies on `assemble` calling this on the way out.
fn cap_cmd(s: String) -> String {
    if s.len() <= 1024 {
        return s;
    }
    let mut end = 1024;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Map one `LayoutNode` subtree into its on-disk `NodeSnap` shape, filling
/// each leaf's live pane data via `meta` (a session id -> `PaneSnap` lookup
/// the caller closes over its own live state with). `last_cmd` is capped
/// here (see `cap_cmd`) so every path that can produce a `PaneSnap` — real
/// wiring and this recursive walk alike — goes through the one truncation
/// point.
fn assemble_node(
    node: &ember_core::layout::LayoutNode,
    meta: &dyn Fn(&ember_core::ids::SessionId) -> PaneSnap,
) -> NodeSnap {
    use ember_core::layout::LayoutNode;
    match node {
        LayoutNode::Pane { session, .. } => {
            let mut pane = meta(session);
            pane.last_cmd = pane.last_cmd.map(cap_cmd);
            NodeSnap::Pane(pane)
        }
        LayoutNode::Split { axis, ratio, a, b } => NodeSnap::Split {
            dir: match axis {
                ember_core::layout::Axis::Horizontal => 'h',
                ember_core::layout::Axis::Vertical => 'v',
            },
            ratio: *ratio as f32,
            a: Box::new(assemble_node(a, meta)),
            b: Box::new(assemble_node(b, meta)),
        },
    }
}

/// One window's live geometry + tree + per-tab `named_by_user` flags, as
/// `assemble` wants them: `(outer position, inner size, tree, named-by-user
/// flags parallel to `tree.tabs`)`. A named alias purely so the tuple isn't
/// spelled out (and clippy's `type_complexity` tripped) at every use site —
/// the tuple shape itself is the real, documented interface.
pub type WindowInput<'a> = (
    Option<(i32, i32)>,
    (u32, u32),
    &'a ember_core::layout::WindowTree,
    &'a [bool],
);

/// Pure assembly: map the live per-window `WindowTree`s (plus each window's
/// `(pos, size)` and per-tab `named_by_user` flags, all already extracted by
/// the caller from winit/`WindowState`) and each pane's live metadata
/// (`meta`, which the wiring layer closes over `Shared::pane_meta`) into the
/// on-disk `SessionSnapshot` shape. No I/O and no dependency on `Shared` or
/// `WindowState`, so it's exhaustively unit-testable here; the impure half
/// (reading winit/`Shared` state, deciding when to call this) lives in
/// `main.rs`'s `session_dirty`.
///
/// `windows[i].3` (the `named_by_user` slice) is indexed in parallel with
/// `windows[i].2.tabs` — a short slice (fewer entries than tabs) treats the
/// missing tail as `false` rather than panicking, so a caller can never
/// crash the app by passing a stale-length flags slice.
pub fn assemble(
    windows: &[WindowInput],
    meta: &dyn Fn(&ember_core::ids::SessionId) -> PaneSnap,
) -> SessionSnapshot {
    let windows = windows
        .iter()
        .map(|(pos, size, tree, named_by_user)| WindowSnap {
            pos: *pos,
            size: *size,
            focused_tab: tree.active,
            tabs: tree
                .tabs
                .iter()
                .enumerate()
                .map(|(i, tab)| TabSnap {
                    name: tab.title.clone(),
                    named_by_user: named_by_user.get(i).copied().unwrap_or(false),
                    splits: assemble_node(&tab.root, meta),
                })
                .collect(),
        })
        .collect();
    let saved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    SessionSnapshot {
        version: 1,
        saved_at,
        windows,
    }
}

/// The session-state file path, if a home/state dir can be determined.
/// Mirrors `config::path()`'s XDG shape.
pub fn state_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("ember/session.json"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/ember/session.json"))
}

/// Write `snap` to `path` atomically: serialize to a same-directory temp
/// file (mode 0600 on unix), fsync it, then rename over the target. A
/// reader never observes a partial or torn write.
pub fn write_atomic(path: &Path, snap: &SessionSnapshot) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(snap)?;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp_path)?;

    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

static WRITE_FAILURE_LOGGED: Once = Once::new();

/// Log a write failure exactly once for the process lifetime, then swallow
/// it. The write policy: never panic, never block a caller; the next update
/// retries naturally.
fn log_write_failure_once(e: &io::Error) {
    WRITE_FAILURE_LOGGED.call_once(|| {
        eprintln!("[ember] session snapshot write failed: {e}");
    });
}

/// Owns the writer thread. Dropping the returned `SnapshotHandle` stops it
/// (after flushing any pending snapshot).
pub struct SnapshotWriter;

impl SnapshotWriter {
    /// Spawn the debounce/writer thread for `path` and return a handle to
    /// feed it snapshots. The thread sleeps indefinitely when idle: it never
    /// wakes on a timer while nothing is pending.
    pub fn spawn(path: PathBuf) -> SnapshotHandle {
        let (tx, rx) = mpsc::channel::<SessionSnapshot>();
        let join = std::thread::spawn(move || {
            // Writer thread: 300ms quiet, 1s max defer, Drop flushes.
            let mut pending: Option<SessionSnapshot> = None;
            let mut first_dirty: Option<Instant> = None;
            loop {
                let timeout = match first_dirty {
                    None => Duration::from_secs(3600), // idle: sleep until a message
                    Some(t0) => {
                        let quiet_deadline = Duration::from_millis(300);
                        let cap_deadline = Duration::from_secs(1).saturating_sub(t0.elapsed());
                        quiet_deadline.min(cap_deadline)
                    }
                };
                match rx.recv_timeout(timeout) {
                    Ok(snap) => {
                        pending = Some(snap);
                        first_dirty.get_or_insert_with(Instant::now);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(s) = pending.take() {
                            if let Err(e) = write_atomic(&path, &s) {
                                log_write_failure_once(&e);
                            }
                        }
                        first_dirty = None;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        if let Some(s) = pending.take() {
                            if let Err(e) = write_atomic(&path, &s) {
                                log_write_failure_once(&e);
                            }
                        }
                        break;
                    }
                }
            }
        });
        SnapshotHandle {
            tx: Some(tx),
            join: Some(join),
        }
    }
}

/// A handle to a running [`SnapshotWriter`] thread. `update()` feeds it a
/// new snapshot (debounced, never blocking the caller); dropping the handle
/// flushes any pending snapshot synchronously before returning.
pub struct SnapshotHandle {
    tx: Option<Sender<SessionSnapshot>>,
    join: Option<JoinHandle<()>>,
}

impl SnapshotHandle {
    /// Queue a new snapshot. Never blocks; a full mailbox slot or a dead
    /// writer thread is silently ignored (the caller must never stall on
    /// this).
    pub fn update(&self, snap: SessionSnapshot) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(snap);
        }
    }
}

impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        // Drop the sender first so the writer thread's `recv_timeout` sees
        // `Disconnected`, flushes any pending snapshot, and exits; then join
        // so that flush has completed before we return.
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(marker: &str) -> SessionSnapshot {
        SessionSnapshot {
            version: 1,
            saved_at: marker.into(),
            windows: vec![],
        }
    }
    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ember-ss-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("session.json")
    }

    #[test]
    fn atomic_write_roundtrips_with_0600() {
        let p = tmp("rt");
        write_atomic(&p, &snap("t1")).unwrap();
        let loaded: SessionSnapshot =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(loaded.saved_at, "t1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn writer_debounces_and_writes_last_value() {
        let p = tmp("debounce");
        let h = SnapshotWriter::spawn(p.clone());
        for i in 0..20 {
            h.update(snap(&format!("v{i}")));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // 20 updates over ~200ms: the 1s cap OR the 300ms quiet after the last
        // update must have produced a file containing the LAST value by now.
        std::thread::sleep(std::time::Duration::from_millis(600));
        let loaded: SessionSnapshot =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(loaded.saved_at, "v19");
        drop(h);
    }

    #[test]
    fn drop_flushes_pending() {
        let p = tmp("flush");
        let h = SnapshotWriter::spawn(p.clone());
        h.update(snap("final"));
        drop(h); // no sleep: Drop must flush synchronously
        let loaded: SessionSnapshot =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(loaded.saved_at, "final");
    }

    #[test]
    fn assemble_maps_tree_and_meta() {
        use ember_core::{
            ids::{PaneId, SessionId, TabId},
            layout::{Axis, LayoutNode, Tab, WindowTree},
        };
        let tree = WindowTree {
            active: 0,
            tabs: vec![Tab {
                id: TabId(1),
                title: "EA".into(),
                focus: PaneId(1),
                root: LayoutNode::split(
                    Axis::Horizontal,
                    0.5,
                    LayoutNode::pane(PaneId(1), SessionId::new("s1")),
                    LayoutNode::pane(PaneId(2), SessionId::new("s2")),
                ),
            }],
        };
        let snap = assemble(
            &[(Some((10, 20)), (800, 600), &tree, &[true])],
            &|sid: &SessionId| PaneSnap {
                cwd: Some(format!("/d/{}", sid.0)),
                last_cmd: (sid.0 == "s1").then(|| "gt crew at skippy".into()),
                was_running: sid.0 == "s1",
            },
        );
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].pos, Some((10, 20)));
        assert_eq!(snap.windows[0].size, (800, 600));
        let tab = &snap.windows[0].tabs[0];
        assert!(tab.named_by_user);
        let NodeSnap::Split { dir, a, .. } = &tab.splits else {
            panic!("expected split")
        };
        assert_eq!(*dir, 'h');
        let NodeSnap::Pane(p) = a.as_ref() else {
            panic!("expected pane")
        };
        assert_eq!(p.last_cmd.as_deref(), Some("gt crew at skippy"));
    }

    #[test]
    fn assemble_caps_last_cmd_at_1024_on_a_char_boundary() {
        use ember_core::ids::{PaneId, SessionId, TabId};
        use ember_core::layout::{LayoutNode, Tab, WindowTree};
        // A multi-byte char (3 bytes each) straddling the 1024 cutoff so a
        // byte-oblivious truncation would split it.
        let long: String = "€".repeat(400); // 1200 bytes
        let tree = WindowTree {
            active: 0,
            tabs: vec![Tab {
                id: TabId(1),
                title: String::new(),
                focus: PaneId(1),
                root: LayoutNode::pane(PaneId(1), SessionId::new("s1")),
            }],
        };
        let snap = assemble(&[(None, (100, 100), &tree, &[false])], &|_sid| PaneSnap {
            cwd: None,
            last_cmd: Some(long.clone()),
            was_running: false,
        });
        let NodeSnap::Pane(p) = &snap.windows[0].tabs[0].splits else {
            panic!("expected pane")
        };
        let capped = p.last_cmd.as_deref().unwrap();
        assert!(capped.len() <= 1024);
        assert!(long.starts_with(capped));
        // Must land ON a char boundary, not mid-character.
        assert!(long.is_char_boundary(capped.len()));
    }
}
