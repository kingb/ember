//! Session snapshot model, atomic writer, and debounce thread.
//!
//! Lives at `$XDG_STATE_HOME/ember/session.json` (else `~/.local/state/ember/session.json`).
//! Defines its own plain serde structs, decoupled from `ember_core`'s in-memory
//! layout types, so the on-disk schema is stable independent of internal
//! refactors. Later tasks feed this a snapshot of the live window/tab/split
//! state on every change; `SnapshotWriter` debounces those updates (300ms
//! quiet, 1s max defer) and writes them atomically so a crash never leaves a
//! corrupt or partial file on disk.
//!
//! This module is a leaf: nothing in `main.rs` builds a `SessionSnapshot` or
//! spawns a `SnapshotWriter` yet. That wiring lands in later tasks (capture,
//! restore-on-launch), so every public item here is temporarily unreferenced
//! from the binary's perspective even though it is exercised by this file's
//! own tests.
#![allow(dead_code)]

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
}
