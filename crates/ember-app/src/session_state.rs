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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Flatten a `NodeSnap` tree into the sequence of split operations that
/// recreates it: `(parent-pane index in creation order, dir, ratio, new
/// pane's PaneSnap)`. The first pane of the tab is index 0 (created with the
/// tab itself); each op creates exactly one more pane, so replaying `ops` in
/// order — `split_pane(pane_by_index[parent], dir, ratio)` — assigns each
/// new pane the NEXT index (1, 2, 3, …) in emission order, matching what
/// this function assumed while building the list.
///
/// Mirrors [`ember_core`]'s `SplitPane` semantics exactly: a split always
/// targets an existing LEAF pane and replaces it in place with a
/// `Split { a: <that pane, unchanged>, b: <fresh pane> }` — the target pane
/// keeps its identity (and position, however deeply nested) on the `a`
/// side; only `b` is new. So to grow a pane at index `at` into a whole
/// `Split { a, b }` subtree: first split `at` itself (`dir`/`ratio`,
/// carrying `b`'s own eventual first pane's data as the new pane's content)
/// — this one op reproduces the split node itself, with both sides still
/// plain leaves — then recurse to grow `a` further AT THE SAME INDEX `at`
/// (its identity survives being wrapped), and `b` further at the index the
/// first op just created. Order between the two recursions doesn't matter
/// (independent subtrees after the first op); this walks `a` before `b`.
pub fn split_ops(root: &NodeSnap) -> (PaneSnap, Vec<(usize, char, f32, PaneSnap)>) {
    let mut ops = Vec::new();
    let mut next_index = 1usize; // index 0 = the tab's own seed pane
    let first = walk_split_ops(root, 0, &mut next_index, &mut ops);
    (first, ops)
}

/// This subtree's leftmost-via-`a` pane's snap, with NO ops emitted — used
/// to peek a fresh split's "new pane" payload before recursing into it for
/// real (see [`split_ops`]'s doc).
fn leftmost_pane(node: &NodeSnap) -> PaneSnap {
    match node {
        NodeSnap::Pane(p) => p.clone(),
        NodeSnap::Split { a, .. } => leftmost_pane(a),
    }
}

/// Recursive helper for [`split_ops`]. `at` is the creation-order index of
/// the pane CURRENTLY occupying this subtree's position (already exists —
/// either the tab's own seed pane, at index 0, or a pane an earlier op just
/// created). Returns this subtree's own first (leftmost-via-`a`) `PaneSnap`
/// — `at`'s eventual identity once every op beneath it has run.
fn walk_split_ops(
    node: &NodeSnap,
    at: usize,
    next_index: &mut usize,
    ops: &mut Vec<(usize, char, f32, PaneSnap)>,
) -> PaneSnap {
    match node {
        NodeSnap::Pane(p) => p.clone(),
        NodeSnap::Split { dir, ratio, a, b } => {
            let new_index = *next_index;
            *next_index += 1;
            ops.push((at, *dir, *ratio, leftmost_pane(b)));
            let first = walk_split_ops(a, at, next_index, ops);
            walk_split_ops(b, new_index, next_index, ops);
            first
        }
    }
}

/// Outcome of attempting to load a session snapshot from disk.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadOutcome {
    /// File does not exist.
    None,
    /// File exists but is corrupt (not valid JSON, or unparseable); it has
    /// been renamed to `session.json.corrupt`.
    Corrupt,
    /// File was successfully loaded and parsed.
    Loaded(SessionSnapshot),
}

/// One entry from `list_archives`: a snapshot file, its timestamp from the
/// filename, and metadata (window/tab counts) extracted during listing.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveEntry {
    pub path: PathBuf,
    pub stamp: String,
    pub windows: usize,
    pub tabs: usize,
}

/// Maximum number of archived snapshots to keep. Older archives are pruned
/// when this limit is exceeded.
pub const MAX_ARCHIVES: usize = 10;

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

/// The filesystem footprint the Settings "Delete saved sessions" action
/// covers, colocated next to `path` (`state_path()`'s result): the live
/// snapshot itself, its quarantined-corrupt sibling (`session.json.corrupt`
/// — Task 7's quarantine path writes it), and every archived
/// `session.json.prev-*` snapshot (Task 7's `archive`). Only entries that
/// actually exist are returned. Shared by the read-only `saved_state_count`
/// (the Settings row's live count) and the destructive `delete_all_state`,
/// so the two can never disagree about what's there.
fn state_footprint(path: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if path.is_file() {
        found.push(path.to_path_buf());
    }
    let corrupt = path.with_extension("json.corrupt");
    if corrupt.is_file() {
        found.push(corrupt);
    }
    if let Some(dir) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("session.json.prev-")
                {
                    found.push(entry.path());
                }
            }
        }
    }
    found
}

/// Count of files the "Delete saved sessions" Settings action would remove,
/// without touching the disk — what the row's `(N)` label and its
/// hidden-when-`0` visibility read. `0` when no state path can be resolved.
pub fn saved_state_count() -> usize {
    state_path().map(|p| state_footprint(&p).len()).unwrap_or(0)
}

/// Delete every on-disk session-state file (see `state_footprint`): the
/// live snapshot, its quarantined-corrupt sibling, and every
/// `session.json.prev-*` archive. Best-effort — a removal failure for one
/// file doesn't stop the others. Returns the count actually removed (`0`
/// when no state path can be resolved or nothing exists), which the
/// Settings row uses to refresh its own count immediately after.
pub fn delete_all_state() -> usize {
    let Some(path) = state_path() else {
        return 0;
    };
    state_footprint(&path)
        .into_iter()
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// Clear every pane's `last_cmd` to `None` in the split tree, in place.
fn strip_node_commands(node: &mut NodeSnap) {
    match node {
        NodeSnap::Pane(p) => p.last_cmd = None,
        NodeSnap::Split { a, b, .. } => {
            strip_node_commands(a);
            strip_node_commands(b);
        }
    }
}

/// Immediately rewrite the state file at `path` with every pane's
/// `last_cmd` cleared — the "Capture commands" off-switch's immediate-strip
/// ruling (privacy: turning capture off also scrubs what's already on
/// disk, not just what gets written next). Load, map, `write_atomic`; a
/// missing file is a no-op (nothing to strip), and an unparseable one is
/// left alone (Task 7's quarantine path owns corrupt files, not this one).
pub fn strip_commands(path: &Path) -> io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let Ok(mut snap) = serde_json::from_str::<SessionSnapshot>(&text) else {
        return Ok(());
    };
    for w in &mut snap.windows {
        for t in &mut w.tabs {
            strip_node_commands(&mut t.splits);
        }
    }
    write_atomic(path, &snap)
}

/// Load and parse a session snapshot from `path`. Returns:
/// - `LoadOutcome::None` if the file does not exist or has an unsupported version.
/// - `LoadOutcome::Corrupt` if the file exists but is not valid JSON; the
///   file is renamed to `session.json.corrupt` (overwriting any older
///   corrupt file).
/// - `LoadOutcome::Loaded(snap)` if the file was successfully parsed.
pub fn load(path: &Path) -> LoadOutcome {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LoadOutcome::None,
        Err(_) => return LoadOutcome::None,
    };

    match serde_json::from_str::<SessionSnapshot>(&text) {
        Ok(snap) => {
            if snap.version != 1 {
                // Unknown version: leave the file alone (it might be from a
                // future version we can't parse yet).
                return LoadOutcome::None;
            }
            LoadOutcome::Loaded(snap)
        }
        Err(_) => {
            // Corrupt JSON: rename to quarantine and return Corrupt.
            let corrupt_path = path.with_extension("json.corrupt");
            if let Err(e) = std::fs::rename(path, &corrupt_path) {
                log_write_failure_once(&e);
            }
            LoadOutcome::Corrupt
        }
    }
}

/// Format a timestamp as YYYYMMDD-HHMMSS from the given SystemTime.
/// Returns an empty string if the time cannot be formatted.
fn format_timestamp(time: SystemTime) -> String {
    let duration = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    let secs = duration.as_secs();

    // Compute YYYYMMDD-HHMMSS from seconds since epoch.
    // This is a simplified calculation that doesn't account for leap seconds.
    const SECS_PER_DAY: u64 = 86400;
    const SECS_PER_HOUR: u64 = 3600;
    const SECS_PER_MINUTE: u64 = 60;

    // Convert seconds to days, hours, minutes, seconds
    let days_since_epoch = secs / SECS_PER_DAY;
    let secs_in_day = secs % SECS_PER_DAY;
    let hours = secs_in_day / SECS_PER_HOUR;
    let secs_in_hour = secs_in_day % SECS_PER_HOUR;
    let minutes = secs_in_hour / SECS_PER_MINUTE;
    let seconds = secs_in_hour % SECS_PER_MINUTE;

    // Days since Jan 1, 1970 to a calendar date (simplified).
    // This is approximate but sufficient for archive timestamps.
    let mut year = 1970;
    let mut days = days_since_epoch;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let mut month = 1;
    let mut day = days + 1;
    for m in 1..=12 {
        let days_in_month = match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap_year(year) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        };
        if day <= days_in_month {
            month = m;
            break;
        }
        day -= days_in_month;
    }

    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        year, month, day, hours, minutes, seconds
    )
}

fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Inverse of `format_timestamp`: parse a `YYYYMMDD-HHMMSS` stamp back into
/// a `SystemTime`. Only the first 15 bytes are read, so an
/// `archive_with_stamp` collision suffix (`-2`, `-3`, …) appended after the
/// base stamp is silently ignored rather than rejected. `None` on anything
/// too short or non-numeric (a hand-edited or truncated filename) — the
/// restore modal falls back to an empty age string rather than panicking on
/// a malformed archive name.
fn parse_timestamp(stamp: &str) -> Option<SystemTime> {
    let core = stamp.get(0..15)?;
    let (date, rest) = core.split_at(8);
    let time = rest.strip_prefix('-')?;
    let year: u64 = date.get(0..4)?.parse().ok()?;
    let month: u64 = date.get(4..6)?.parse().ok()?;
    let day: u64 = date.get(6..8)?.parse().ok()?;
    let hour: u64 = time.get(0..2)?.parse().ok()?;
    let minute: u64 = time.get(2..4)?.parse().ok()?;
    let second: u64 = time.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap_year(year) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        };
    }
    days += day - 1;
    let secs = days * SECS_PER_DAY + hour * 3600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

const SECS_PER_DAY: u64 = 86400;

/// Humanize an elapsed duration (seconds) into a short, casual age label —
/// the restore modal's own scale, not a calendar-accurate diff: "just now",
/// "5m ago", "2h ago", "3d ago", "3 weeks ago", "2 months ago", "1 year
/// ago". A negative (clock-skew) or unparseable elapsed time also reads as
/// "just now" rather than a confusing negative duration.
fn humanize_duration(secs: i64) -> String {
    if secs < 60 {
        return "just now".to_string();
    }
    let secs = secs as u64;
    if secs < 3600 {
        return format!("{}m ago", secs / 60);
    }
    if secs < SECS_PER_DAY {
        return format!("{}h ago", secs / 3600);
    }
    if secs < SECS_PER_DAY * 7 {
        return format!("{}d ago", secs / SECS_PER_DAY);
    }
    if secs < SECS_PER_DAY * 30 {
        let w = secs / (SECS_PER_DAY * 7);
        return format!("{w} week{} ago", if w == 1 { "" } else { "s" });
    }
    if secs < SECS_PER_DAY * 365 {
        let mo = secs / (SECS_PER_DAY * 30);
        return format!("{mo} month{} ago", if mo == 1 { "" } else { "s" });
    }
    let y = secs / (SECS_PER_DAY * 365);
    format!("{y} year{} ago", if y == 1 { "" } else { "s" })
}

/// Humanize a `SessionSnapshot::saved_at` (unix-seconds-as-string) relative
/// to `now`. An unparseable/missing `saved_at` (a hand-edited or ancient
/// snapshot) reads as "just now" — a stale-looking timestamp would be
/// actively misleading in the restore prompt, and "just now" is the neutral
/// fallback the humanized scale already provides.
pub fn humanize_age(saved_at: &str, now: SystemTime) -> String {
    let Ok(secs) = saved_at.parse::<u64>() else {
        return "just now".to_string();
    };
    let saved = UNIX_EPOCH + Duration::from_secs(secs);
    let elapsed = now
        .duration_since(saved)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    humanize_duration(elapsed)
}

/// Humanize an archive's `YYYYMMDD-HHMMSS` filename stamp relative to `now`
/// (the `Older…` list's per-row age) — same fallback-to-"just now" ruling
/// as `humanize_age` for a stamp that fails to parse.
pub fn humanize_stamp(stamp: &str, now: SystemTime) -> String {
    let Some(saved) = parse_timestamp(stamp) else {
        return "just now".to_string();
    };
    let elapsed = now
        .duration_since(saved)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    humanize_duration(elapsed)
}

/// Archive the snapshot at `path` by renaming it to
/// `session.json.prev-<YYYYMMDD-HHMMSS>`. Then prune old archives, keeping
/// only the `MAX_ARCHIVES` newest by filename (which sorts by timestamp).
pub fn archive(path: &Path) -> io::Result<()> {
    let stamp = format_timestamp(SystemTime::now());
    archive_with_stamp(path, &stamp)
}

/// Archive with an explicit stamp (for testing). Renames `path` to
/// `session.json.prev-<stamp>`, then prunes old archives to keep only
/// the `MAX_ARCHIVES` newest by timestamp. If a file with that stamp
/// already exists, appends a numeric suffix (`-2`, `-3`, etc.) to avoid
/// collisions; the suffixed name still sorts lexicographically as newer.
pub fn archive_with_stamp(path: &Path, stamp: &str) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    // Rename to archive, with collision avoidance via numeric suffix
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    let mut archive_path = dir.join(format!("session.json.prev-{}", stamp));
    let mut suffix = 2;
    while archive_path.exists() {
        archive_path = dir.join(format!("session.json.prev-{}-{}", stamp, suffix));
        suffix += 1;
    }
    std::fs::rename(path, archive_path)?;

    // Prune old archives
    prune_archives(dir)?;
    Ok(())
}

/// Prune archived snapshots in `dir` to keep only the `MAX_ARCHIVES` newest.
fn prune_archives(dir: &Path) -> io::Result<()> {
    let mut archives = match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    if path
                        .file_name()?
                        .to_string_lossy()
                        .starts_with("session.json.prev-")
                    {
                        Some(path)
                    } else {
                        None
                    }
                })
                .collect();
            found.sort_by(|a, b| b.cmp(a)); // Sort newest first
            found
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    // Remove oldest archives beyond MAX_ARCHIVES
    while archives.len() > MAX_ARCHIVES {
        if let Some(path) = archives.pop() {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// List all archived snapshots in `dir`, returning the newest first.
/// Each entry contains the path, extracted timestamp, window count, and tab count.
/// Unparseable files are skipped.
pub fn list_archives(dir: &Path) -> Vec<ArchiveEntry> {
    let mut entries: Vec<ArchiveEntry> = match std::fs::read_dir(dir) {
        Ok(dir_entries) => {
            dir_entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    let file_name = path.file_name()?.to_string_lossy().to_string();
                    if !file_name.starts_with("session.json.prev-") {
                        return None;
                    }

                    // Extract stamp from filename
                    let stamp = file_name
                        .strip_prefix("session.json.prev-")
                        .unwrap_or("")
                        .to_string();

                    // Read and parse the file to count windows and tabs
                    let text = std::fs::read_to_string(&path).ok()?;
                    let snap: SessionSnapshot = serde_json::from_str(&text).ok()?;

                    let mut total_tabs = 0;
                    for window in &snap.windows {
                        total_tabs += window.tabs.len();
                    }

                    Some(ArchiveEntry {
                        path,
                        stamp,
                        windows: snap.windows.len(),
                        tabs: total_tabs,
                    })
                })
                .collect()
        }
        Err(_) => return Vec::new(),
    };

    entries.sort_by(|a, b| b.stamp.cmp(&a.stamp)); // Sort newest first
    entries
}

/// Read and parse one archived snapshot for restoring — unlike `load`, this
/// never quarantines a bad file (an archive is disposable/read-only history,
/// not the live state `load`'s corruption handling protects) and never
/// checks `version` (an archive this old process can't parse is simply
/// unusable here). `None` on any read or parse failure.
pub fn load_archive(path: &Path) -> Option<SessionSnapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
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
    fn pane(cwd: &str) -> PaneSnap {
        PaneSnap {
            cwd: Some(cwd.to_string()),
            last_cmd: None,
            was_running: false,
        }
    }

    #[test]
    fn split_ops_recreates_a_nested_tree() {
        // h-split whose left is a v-split: ops must be executable in order.
        let tree = NodeSnap::Split {
            dir: 'h',
            ratio: 0.6,
            a: Box::new(NodeSnap::Split {
                dir: 'v',
                ratio: 0.5,
                a: Box::new(NodeSnap::Pane(pane("p0"))),
                b: Box::new(NodeSnap::Pane(pane("p1"))),
            }),
            b: Box::new(NodeSnap::Pane(pane("p2"))),
        };
        let (first, ops) = split_ops(&tree);
        assert_eq!(first.cwd.as_deref(), Some("p0"));
        assert_eq!(ops.len(), 2);
        // op order must split the root h first (p2 off p0), then v (p1 off p0),
        // OR any order that yields the same final tree — assert the invariant:
        assert_eq!(ops.iter().filter(|o| o.1 == 'h').count(), 1);
        assert_eq!(ops.iter().filter(|o| o.1 == 'v').count(), 1);
    }

    #[test]
    fn split_ops_on_a_bare_pane_is_the_seed_with_no_ops() {
        let (first, ops) = split_ops(&NodeSnap::Pane(pane("only")));
        assert_eq!(first.cwd.as_deref(), Some("only"));
        assert!(ops.is_empty());
    }

    /// Every op's parent index must be resolvable by replaying ops in order
    /// against a flat `pane_by_index` vec seeded with just the first pane —
    /// exactly how `main.rs`'s `spawn_restored` walks them. This simulates
    /// that replay purely (no real split_pane/tree) and checks the resulting
    /// parent/child pane-content adjacency matches a hand-built expectation
    /// for a 3-level-deep tree (more ops than the 2-op nested-tree test).
    #[test]
    fn split_ops_replay_indices_resolve_for_a_three_level_tree() {
        // ((p0 | p1) over p2) beside p3:  h( v( h(p0,p1), p2 ), p3 )
        let tree = NodeSnap::Split {
            dir: 'h',
            ratio: 0.5,
            a: Box::new(NodeSnap::Split {
                dir: 'v',
                ratio: 0.5,
                a: Box::new(NodeSnap::Split {
                    dir: 'h',
                    ratio: 0.5,
                    a: Box::new(NodeSnap::Pane(pane("p0"))),
                    b: Box::new(NodeSnap::Pane(pane("p1"))),
                }),
                b: Box::new(NodeSnap::Pane(pane("p2"))),
            }),
            b: Box::new(NodeSnap::Pane(pane("p3"))),
        };
        let (first, ops) = split_ops(&tree);
        assert_eq!(first.cwd.as_deref(), Some("p0"));
        assert_eq!(ops.len(), 3);
        // Replay: pane_by_index[0] = first; each op's parent index must
        // already exist in the vec built so far.
        let mut pane_by_index = vec![first.cwd.clone()];
        for (parent, _dir, _ratio, new_pane) in &ops {
            assert!(
                *parent < pane_by_index.len(),
                "op parent index {parent} not yet created"
            );
            pane_by_index.push(new_pane.cwd.clone());
        }
        // Every original pane's cwd must appear exactly once across the
        // replay (the seed + one per op).
        let mut cwds: Vec<Option<String>> = pane_by_index;
        cwds.sort();
        let mut expected = vec![
            Some("p0".to_string()),
            Some("p1".to_string()),
            Some("p2".to_string()),
            Some("p3".to_string()),
        ];
        expected.sort();
        assert_eq!(cwds, expected);
    }

    // --- humanize_age / humanize_stamp / parse_timestamp -----

    #[test]
    fn humanize_age_buckets() {
        let now = UNIX_EPOCH + Duration::from_secs(3_000_000_000);
        let saved = |ago: u64| {
            (now - Duration::from_secs(ago))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string()
        };
        assert_eq!(humanize_age(&saved(5), now), "just now");
        assert_eq!(humanize_age(&saved(60 * 5), now), "5m ago");
        assert_eq!(humanize_age(&saved(3600 * 2), now), "2h ago");
        assert_eq!(humanize_age(&saved(86400 * 3), now), "3d ago");
        assert_eq!(humanize_age(&saved(86400 * 7 * 3), now), "3 weeks ago");
        assert_eq!(humanize_age(&saved(86400 * 30), now), "1 month ago");
        assert_eq!(humanize_age(&saved(86400 * 365 * 2), now), "2 years ago");
        assert_eq!(humanize_age("not-a-number", now), "just now");
    }

    #[test]
    fn parse_timestamp_round_trips_format_timestamp() {
        let t = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let stamp = format_timestamp(t);
        let parsed = parse_timestamp(&stamp).unwrap();
        // Round-trips to the second (format_timestamp truncates to seconds).
        assert_eq!(
            parsed.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            t.duration_since(UNIX_EPOCH).unwrap().as_secs()
        );
    }

    #[test]
    fn parse_timestamp_ignores_a_collision_suffix() {
        let t = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let stamp = format!("{}-2", format_timestamp(t));
        let parsed = parse_timestamp(&stamp).unwrap();
        assert_eq!(
            parsed.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            t.duration_since(UNIX_EPOCH).unwrap().as_secs()
        );
    }

    #[test]
    fn parse_timestamp_rejects_garbage() {
        assert!(parse_timestamp("not-a-stamp").is_none());
        assert!(parse_timestamp("").is_none());
    }

    #[test]
    fn humanize_stamp_matches_humanize_age_for_the_same_instant() {
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let saved = now - Duration::from_secs(3600 * 5);
        let stamp = format_timestamp(saved);
        assert_eq!(humanize_stamp(&stamp, now), "5h ago");
    }

    // --- load_archive -----

    #[test]
    fn load_archive_reads_a_valid_file_and_none_for_missing_or_bad() {
        let p = tmp("load-archive");
        write_atomic(&p, &snap("archived")).unwrap();
        let loaded = load_archive(&p).unwrap();
        assert_eq!(loaded.saved_at, "archived");
        assert!(load_archive(&p.with_file_name("nope.json")).is_none());
        let bad = p.with_file_name("bad.json");
        std::fs::write(&bad, b"not json").unwrap();
        assert!(load_archive(&bad).is_none());
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

    // --- strip_commands: the "Capture commands" off immediate-strip -------

    fn snap_with_commands() -> SessionSnapshot {
        SessionSnapshot {
            version: 1,
            saved_at: "t1".into(),
            windows: vec![WindowSnap {
                pos: Some((1, 2)),
                size: (80, 24),
                focused_tab: 0,
                tabs: vec![TabSnap {
                    name: "tab".into(),
                    named_by_user: true,
                    splits: NodeSnap::Split {
                        dir: 'h',
                        ratio: 0.5,
                        a: Box::new(NodeSnap::Pane(PaneSnap {
                            cwd: Some("/a".into()),
                            last_cmd: Some("echo left".into()),
                            was_running: true,
                        })),
                        b: Box::new(NodeSnap::Pane(PaneSnap {
                            cwd: Some("/b".into()),
                            last_cmd: Some("echo right".into()),
                            was_running: false,
                        })),
                    },
                }],
            }],
        }
    }

    #[test]
    fn strip_commands_clears_last_cmd_but_keeps_everything_else() {
        let p = tmp("strip");
        write_atomic(&p, &snap_with_commands()).unwrap();
        strip_commands(&p).unwrap();
        let loaded: SessionSnapshot =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let NodeSnap::Split { a, b, dir, ratio } = &loaded.windows[0].tabs[0].splits else {
            panic!("expected split")
        };
        let NodeSnap::Pane(pa) = a.as_ref() else {
            panic!("expected pane")
        };
        let NodeSnap::Pane(pb) = b.as_ref() else {
            panic!("expected pane")
        };
        assert_eq!(pa.last_cmd, None);
        assert_eq!(pb.last_cmd, None);
        // Everything else survives untouched.
        assert_eq!(*dir, 'h');
        assert_eq!(*ratio, 0.5);
        assert_eq!(pa.cwd.as_deref(), Some("/a"));
        assert!(pa.was_running);
        assert_eq!(pb.cwd.as_deref(), Some("/b"));
        assert!(!pb.was_running);
        assert!(loaded.windows[0].tabs[0].named_by_user);
    }

    /// The full "Capture commands off must actually keep commands off disk"
    /// regression, not just the on-disk strip: `strip_commands` above only
    /// rewrites the FILE as it stands the instant capture is toggled off.
    /// The real bug was that the next `session_dirty` reassembled from
    /// `Shared::pane_meta` — which the strip never touches — and silently
    /// wrote every live command straight back on top of the file that was
    /// just stripped. This exercises the other two legs of the fix against
    /// the real `assemble` pure function: `crate::clear_captured_commands`
    /// scrubbing pane metadata that (like the bug) still has a command sitting
    /// in it, and `crate::pane_snap_for` gating `last_cmd` on
    /// `capture_commands` as a second, independent line of defense — so a
    /// subsequent assemble carries no commands whether or not the metadata
    /// was scrubbed in time.
    #[test]
    fn subsequent_assemble_carries_no_commands_once_capture_is_off() {
        use ember_core::ids::{PaneId, SessionId, TabId};
        use ember_core::layout::{LayoutNode, Tab, WindowTree};

        let p = tmp("strip-then-reassemble");
        write_atomic(&p, &snap_with_commands()).unwrap();
        strip_commands(&p).unwrap();

        let sid = SessionId::new("s1");
        let tree = WindowTree {
            active: 0,
            tabs: vec![Tab {
                id: TabId(1),
                title: String::new(),
                focus: PaneId(1),
                root: LayoutNode::pane(PaneId(1), sid.clone()),
            }],
        };

        // Live metadata that a race (or a missed call site) left un-scrubbed
        // at toggle time, same as the bug: a command still sitting in
        // `pane_meta` when the next dirty event fires.
        let mut pane_meta = std::collections::HashMap::new();
        pane_meta.insert(
            sid.clone(),
            crate::PaneMeta {
                cwd: Some("/a".to_string()),
                last_cmd: Some("echo left".to_string()),
                was_running: true,
            },
        );

        // Leg (a): the settings-effect handler scrubs it directly.
        crate::clear_captured_commands(&mut pane_meta);

        // Leg (b): `pane_snap_for`'s gate is the belt-and-braces backstop —
        // still holds even if a future call site skips leg (a).
        let capture_commands = false;
        let snap = assemble(&[(None, (80, 24), &tree, &[false])], &|s| {
            crate::pane_snap_for(pane_meta.get(s), capture_commands)
        });

        let NodeSnap::Pane(p) = &snap.windows[0].tabs[0].splits else {
            panic!("expected pane")
        };
        assert_eq!(p.last_cmd, None);
        // cwd and was_running are unrelated to the command-privacy fix and
        // must survive.
        assert_eq!(p.cwd.as_deref(), Some("/a"));
        assert!(p.was_running);
    }

    #[test]
    fn strip_commands_is_a_noop_when_file_is_missing() {
        let p = tmp("strip-missing").with_file_name("does-not-exist.json");
        strip_commands(&p).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn strip_commands_leaves_a_corrupt_file_alone() {
        let p = tmp("strip-corrupt");
        std::fs::write(&p, b"not json").unwrap();
        strip_commands(&p).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"not json");
    }

    // --- delete_all_state / saved_state_count: env-gated by XDG_STATE_HOME -

    /// `state_path()` reads process-wide env vars and `cargo test` runs
    /// multiple tests concurrently on threads within the same process —
    /// serialize every test that points `XDG_STATE_HOME` somewhere so they
    /// can't stomp on each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point `XDG_STATE_HOME` at a fresh scratch dir for the duration of
    /// `f`, restoring the prior value (or unsetting it) afterward. Holds
    /// `ENV_LOCK` for the whole call so no other env-touching test can
    /// interleave.
    fn with_scratch_state_home<R>(name: &str, f: impl FnOnce(&std::path::Path) -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("ember-ss-xdg-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prior = std::env::var_os("XDG_STATE_HOME");
        // Safe: serialized by `ENV_LOCK` above, so no other thread reads or
        // writes process env vars while this is set.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &dir);
        }
        let result = f(&dir);
        #[allow(unsafe_code)]
        unsafe {
            match &prior {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        result
    }

    #[test]
    fn saved_state_count_and_delete_cover_the_live_file_corrupt_sibling_and_archives() {
        with_scratch_state_home("full", |dir| {
            let path = dir.join("ember/session.json");
            write_atomic(&path, &snap("live")).unwrap();
            std::fs::write(path.with_extension("json.corrupt"), b"garbage").unwrap();
            std::fs::write(
                path.with_file_name("session.json.prev-20260101-000000"),
                b"{}",
            )
            .unwrap();
            std::fs::write(
                path.with_file_name("session.json.prev-20260102-000000"),
                b"{}",
            )
            .unwrap();
            // An unrelated file in the same directory must survive untouched.
            std::fs::write(path.with_file_name("unrelated.txt"), b"keep me").unwrap();

            assert_eq!(saved_state_count(), 4);
            let removed = delete_all_state();
            assert_eq!(removed, 4);
            assert_eq!(saved_state_count(), 0);
            assert!(!path.exists());
            assert!(!path.with_extension("json.corrupt").exists());
            assert!(
                !path
                    .with_file_name("session.json.prev-20260101-000000")
                    .exists()
            );
            assert!(path.with_file_name("unrelated.txt").exists());
        });
    }

    #[test]
    fn saved_state_count_is_zero_when_nothing_saved() {
        with_scratch_state_home("empty", |_dir| {
            assert_eq!(saved_state_count(), 0);
            assert_eq!(delete_all_state(), 0);
        });
    }

    // --- load / archive / list_archives tests -----

    #[test]
    fn load_missing_is_none_and_corrupt_is_quarantined() {
        let p = tmp("load");
        assert!(matches!(load(&p), LoadOutcome::None));
        std::fs::write(&p, b"{not json").unwrap();
        assert!(matches!(load(&p), LoadOutcome::Corrupt));
        assert!(!p.exists());
        assert!(p.with_extension("json.corrupt").exists());
    }

    #[test]
    fn unknown_version_is_treated_as_none() {
        let p = tmp("ver");
        std::fs::write(&p, r#"{"version": 99, "saved_at": "x", "windows": []}"#).unwrap();
        assert!(matches!(load(&p), LoadOutcome::None));
        // File must remain untouched for a future version to potentially recover it
        assert!(p.exists());
    }

    #[test]
    fn archive_prunes_to_ten() {
        let p = tmp("arch");
        let parent = p.parent().unwrap();

        // Create 12 archives with distinct stamps
        for i in 0..12 {
            write_atomic(&p, &snap(&format!("s{i}"))).unwrap();
            let stamp = format!("20260801-0000{:02}", i);
            archive_with_stamp(&p, &stamp).unwrap();
        }

        let list = list_archives(parent);
        assert_eq!(list.len(), 10, "Expected 10 archives, got {}", list.len());
        // Verify newest first: stamps are lexicographically ordered, so newest
        // has the highest last digits
        assert!(
            list[0].stamp > list[9].stamp,
            "Archives not sorted newest first"
        );
    }

    #[test]
    fn archive_collision_avoidance_with_numeric_suffix() {
        let p = tmp("collision");
        let parent = p.parent().unwrap();
        let stamp = "20260801-123456";

        // Archive twice with the same stamp
        write_atomic(&p, &snap("first")).unwrap();
        archive_with_stamp(&p, stamp).unwrap();

        write_atomic(&p, &snap("second")).unwrap();
        archive_with_stamp(&p, stamp).unwrap();

        // Both archives should exist with distinct names
        let list = list_archives(parent);
        assert_eq!(
            list.len(),
            2,
            "Expected 2 archives after collision avoidance, got {}",
            list.len()
        );

        // Verify newest first (the -2 suffix sorts after the base stamp)
        assert!(list[0].stamp > list[1].stamp);

        // One entry should have base stamp, the other should have -2 suffix
        let stamps: Vec<&String> = list.iter().map(|e| &e.stamp).collect();
        assert!(
            stamps.contains(&&stamp.to_string())
                || stamps.iter().any(|s| s.as_str() == &format!("{}-2", stamp)),
            "Expected base or -2 suffix stamp"
        );
    }

    #[test]
    fn list_archives_skips_unparseable_prev_files() {
        let p = tmp("unparseable");
        let parent = p.parent().unwrap();

        // Create one valid archive
        write_atomic(&p, &snap("valid")).unwrap();
        archive_with_stamp(&p, "20260801-100000").unwrap();

        // Create an unparseable prev-* file
        std::fs::write(
            parent.join("session.json.prev-20260801-110000"),
            b"this is not json",
        )
        .unwrap();

        let list = list_archives(parent);
        // Should only contain the valid archive, not the unparseable one
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].stamp, "20260801-100000");
    }
}
