//! The alacritty → neutral-grid projection (design §4; ). Owns the
//! `alacritty_terminal::Term`, feeds it PTY bytes, and drains its native damage
//! into an owned [`GridDelta`] of resolved [`NeutralCell`]s. This is the v1 arm
//! of the swappable-engine contract; a `libghostty` projection would be the
//! phase-2 arm, differing only here.

use std::collections::HashMap;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as AlacCursorShape, Processor};
use ember_core::{
    Attrs, CellContent, CellPatch, CursorShape, CursorState, GridDelta, GridDims, MarkStatus,
    NeutralCell, OscEvent, ScrollAmount, Style, StyleId, VtProjection,
};

use crate::osc133::Osc133;

use crate::palette::Palette;

/// Assigns small dense [`StyleId`]s to distinct [`Style`]s and tracks which were
/// first seen in the current delta (design §4: render keys its raster cache on
/// `(glyph, StyleId)`).
struct StyleInterner {
    map: HashMap<Style, StyleId>,
    next: u32,
}

impl StyleInterner {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next: 0,
        }
    }

    /// The complete table, for a resync delta (consumer lost its cache).
    fn all(&self) -> Vec<(StyleId, Style)> {
        let mut v: Vec<(StyleId, Style)> = self.map.iter().map(|(s, id)| (*id, *s)).collect();
        v.sort_unstable_by_key(|(id, _)| *id);
        v
    }

    fn intern(&mut self, style: Style, first_seen: &mut Vec<(StyleId, Style)>) -> StyleId {
        if let Some(id) = self.map.get(&style) {
            *id
        } else {
            let id = StyleId(self.next);
            self.next += 1;
            self.map.insert(style, id);
            first_seen.push((id, style));
            id
        }
    }
}

/// A projection driving an `alacritty_terminal::Term` from bytes into the neutral
/// grid. Generic over the engine's `EventListener` (the LocalPty backend supplies
/// one that forwards semantic events; tests use a no-op).
pub struct AlacrittyProjection<L: EventListener> {
    term: Term<L>,
    parser: Processor,
    interner: StyleInterner,
    palette: Palette,
    dims: GridDims,
    epoch: u64,
    /// Lines the display is scrolled up from the live bottom (`0` = bottom).
    /// Mirrors the engine's `display_offset`; read fresh each drain so it stays in
    /// sync whether it changed via a scroll command or output pushing history.
    display_offset: usize,
    /// Command marks (OSC 133 prompts) and manual marks (OSC 1337 `SetMark`),
    /// in emit order — both navigate and gutter the same way. Each is anchored
    /// to an absolute history line (`history_size + prompt row` at emit) so it
    /// scrolls with content — valid until scrollback saturates the history cap
    /// (a long session), then oldest marks may drift; see the module note.
    marks: Vec<Mark>,
    /// Next drain emits a full reset + the complete style table
    /// ([`ember_core::BackendControl::RequestFull`]).
    resync: bool,
    /// Tail of the previous read that may be a split OSC 133 sequence (its
    /// bytes were already fed to the engine; kept only for the scanner).
    scan_tail_133: Vec<u8>,
    /// Same, for OSC 1337.
    scan_tail_1337: Vec<u8>,
    /// Live scrollback-search state: the pattern as typed, its compiled DFA,
    /// and the engine-space range of the last match (the origin the next
    /// `search` call continues from). Reset when the pattern changes.
    search: Option<SearchState>,
}

/// See `AlacrittyProjection::search`.
struct SearchState {
    pattern: String,
    /// All matches for `pattern`, in buffer order (oldest history -> newest),
    /// capped at [`SEARCH_MATCH_CAP`]. Empty = the pattern matched nothing.
    matches: Vec<(Point, Point)>,
    /// Currently selected match (index into `matches`), if any.
    idx: Option<usize>,
}

/// Upper bound on matches enumerated per query — bounds the cost of the full
/// scan on a huge scrollback with a pathologically common pattern (e.g. "e").
/// Beyond this the count reads as "CAP+"; the normal case is well under it.
const SEARCH_MATCH_CAP: usize = 2000;

/// One tracked command (OSC 133) or manual (OSC 1337 `SetMark`) mark: the
/// prompt/mark's absolute history line, and — for a command — its exit code.
struct Mark {
    abs_line: i64,
    exit: Option<i32>,
    manual: bool,
}

/// A mark from either scanner, tagged so the merged, buffer-ordered pass
/// (`AlacrittyProjection::advance`) can dispatch on its origin.
enum Scanned {
    C133(Osc133),
    C1337(crate::osc1337::Osc1337),
}

impl<L: EventListener> AlacrittyProjection<L> {
    pub fn new(dims: GridDims, listener: L) -> Self {
        let size = TermSize::new(dims.columns as usize, dims.screen_lines as usize);
        let term = Term::new(Config::default(), &size, listener);
        Self {
            term,
            parser: Processor::new(),
            interner: StyleInterner::new(),
            palette: Palette::dark(),
            dims,
            epoch: 0,
            display_offset: 0,
            search: None,
            marks: Vec::new(),
            resync: false,
            scan_tail_133: Vec::new(),
            scan_tail_1337: Vec::new(),
        }
    }

    /// Feed raw PTY bytes through the VT parser into the engine, and pre-scan them
    /// for OSC 133 + OSC 1337 shell-integration sequences (alacritty ignores both,
    /// so the bytes still flow through unchanged). Returns the semantic events
    /// found, in order, and records prompt/exit/manual-mark state for the gutter.
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        // Scan over (carried tail ++ new bytes): a mark split across the 8 KB
        // read boundary is statistically inevitable in long sessions and used
        // to be lost forever. The tail's bytes were already fed to the engine
        // last read — the carry exists only for the scanner. OSC 133 and OSC
        // 1337 are scanned independently (each with its own carry), then
        // merged back into buffer order below — carrying a wrong shared tail
        // would either re-feed already-consumed bytes or duplicate a mark.
        let tail_len_133 = self.scan_tail_133.len();
        let owned_133: Vec<u8>;
        let scan_buf_133: &[u8] = if tail_len_133 == 0 {
            bytes
        } else {
            let mut v = std::mem::take(&mut self.scan_tail_133);
            v.extend_from_slice(bytes);
            owned_133 = v;
            &owned_133
        };
        let tail_len_1337 = self.scan_tail_1337.len();
        let owned_1337: Vec<u8>;
        let scan_buf_1337: &[u8] = if tail_len_1337 == 0 {
            bytes
        } else {
            let mut v = std::mem::take(&mut self.scan_tail_1337);
            v.extend_from_slice(bytes);
            owned_1337 = v;
            &owned_1337
        };
        let result_133 = crate::osc133::scan_split(scan_buf_133);
        let result_1337 = crate::osc1337::scan_split(scan_buf_1337);

        // Both scans' offsets are relative to their OWN scan buffer (which may
        // carry a different-length tail); rebase each to "offset within this
        // read's `bytes`" before merging, so the combined list is in true
        // buffer order regardless of which protocol's tail was longer.
        let mut merged: Vec<(usize, Scanned)> =
            Vec::with_capacity(result_133.marks.len() + result_1337.marks.len());
        merged.extend(result_133.marks.into_iter().map(|(past, m)| {
            (
                past.saturating_sub(tail_len_133).min(bytes.len()),
                Scanned::C133(m),
            )
        }));
        merged.extend(result_1337.marks.into_iter().map(|(past, m)| {
            (
                past.saturating_sub(tail_len_1337).min(bytes.len()),
                Scanned::C1337(m),
            )
        }));
        merged.sort_by_key(|(off, _)| *off);

        let mut cut = 0usize;
        for (feed_to, m) in merged {
            // Feed the engine up to and including this (invisible) mark, so the
            // cursor sits exactly where the mark was emitted. Offset 0 (a mark
            // resolved from a carried tail) means "already fed last read" — feed
            // nothing new for it.
            self.parser
                .advance(&mut self.term, &bytes[cut..feed_to.max(cut)]);
            cut = feed_to.max(cut);
            match m {
                Scanned::C133(Osc133::PromptStart) => {
                    let abs = self.term.grid().history_size() as i64
                        + self.term.grid().cursor.point.line.0.max(0) as i64;
                    self.marks.push(Mark {
                        abs_line: abs,
                        exit: None,
                        manual: false,
                    });
                    if self.marks.len() > 512 {
                        self.marks.remove(0);
                    }
                    events.push(OscEvent::PromptStart);
                }
                Scanned::C133(Osc133::CommandStart) => events.push(OscEvent::CommandStart),
                Scanned::C133(Osc133::OutputStart) => events.push(OscEvent::OutputStart),
                Scanned::C133(Osc133::CommandEnd(code)) => {
                    // The exit belongs to the command whose prompt was the last mark.
                    if let Some(last) = self.marks.last_mut() {
                        last.exit = code;
                    }
                    events.push(OscEvent::CommandEnd(code));
                }
                Scanned::C1337(crate::osc1337::Osc1337::CurrentDir(path)) => {
                    events.push(OscEvent::CurrentDir(path));
                }
                Scanned::C1337(crate::osc1337::Osc1337::RemoteHost(host)) => {
                    events.push(OscEvent::RemoteHost(host));
                }
                Scanned::C1337(crate::osc1337::Osc1337::SetMark) => {
                    let abs = self.term.grid().history_size() as i64
                        + self.term.grid().cursor.point.line.0.max(0) as i64;
                    self.marks.push(Mark {
                        abs_line: abs,
                        exit: None,
                        manual: true,
                    });
                    if self.marks.len() > 512 {
                        self.marks.remove(0);
                    }
                    events.push(OscEvent::SetMark);
                }
            }
        }
        // Feed the remainder.
        self.parser.advance(&mut self.term, &bytes[cut..]);
        // Carry a plausible split-sequence suffix into the next read (bounded —
        // scan_split already rejects oversized garbage as malformed).
        self.scan_tail_133.clear();
        if let Some(inc) = result_133.incomplete {
            self.scan_tail_133.extend_from_slice(&scan_buf_133[inc..]);
        }
        self.scan_tail_1337.clear();
        if let Some(inc) = result_1337.incomplete {
            self.scan_tail_1337.extend_from_slice(&scan_buf_1337[inc..]);
        }
        events
    }

    /// Jump the viewport to the previous (`dir < 0`, older/up) / next (`dir > 0`,
    /// newer/down) prompt mark. No-op on the alt screen or with no marks.
    pub fn scroll_to_prompt(&mut self, dir: i8) {
        if self.term.mode().contains(TermMode::ALT_SCREEN) || self.marks.is_empty() {
            return;
        }
        let hist = self.term.grid().history_size() as i64;
        let screen = self.dims.screen_lines as i64;
        let bottom_abs = hist + screen - 1;
        // Absolute line currently at the top of the viewport.
        let cur_top = bottom_abs - (screen - 1) - self.display_offset as i64;
        // Candidate prompt lines (sorted ascending by abs_line).
        let mut lines: Vec<i64> = self.marks.iter().map(|m| m.abs_line).collect();
        lines.sort_unstable();
        let target = if dir < 0 {
            lines.iter().rev().find(|&&l| l < cur_top).copied()
        } else {
            lines.iter().find(|&&l| l > cur_top).copied()
        };
        if let Some(t) = target {
            // Put the target prompt at the top of the viewport.
            let offset = (bottom_abs - (screen - 1) - t).clamp(0, hist);
            self.scroll(ScrollAmount::To(offset.min(u16::MAX as i64) as u16));
        }
    }

    /// Search scrollback + screen for `pattern` (regex; smart-case: an
    /// all-lowercase pattern matches case-insensitively), continuing one cell
    /// past the previous match (wrapping around the buffer), or starting from
    /// the viewport top on a fresh pattern. Scrolls the display so the match
    /// is visible and returns its scrollback-absolute `(line, col)` range.
    /// `None` for no match, an invalid regex, or an empty pattern (which also
    /// clears the search state).
    pub fn search(&mut self, pattern: &str, forward: bool) -> Option<ember_core::SearchHit> {
        if pattern.is_empty() {
            self.search = None;
            return None;
        }
        let hist = self.term.grid().history_size();
        let screen = self.dims.screen_lines as i32;
        let last_col = self.dims.columns.saturating_sub(1) as usize;
        let display_offset = self.term.grid().display_offset() as i32;

        if self.search.as_ref().map(|s| s.pattern.as_str()) != Some(pattern) {
            // New pattern: compile + enumerate every match once. Smart-case:
            // an all-lowercase pattern matches case-insensitively.
            let needle = if pattern.chars().any(|c| c.is_uppercase()) {
                pattern.to_string()
            } else {
                format!("(?i){pattern}")
            };
            let mut regex = match RegexSearch::new(&needle) {
                Ok(r) => r,
                Err(_) => {
                    self.search = None;
                    return None; // invalid regex: report as no match
                }
            };
            let matches = self.enumerate_matches(&mut regex, last_col, screen, hist);
            // Start on the first match at or below the current viewport top, so
            // a fresh search lands near what you're looking at; else the first.
            let idx = if matches.is_empty() {
                None
            } else {
                let top = -display_offset;
                Some(matches.iter().position(|(s, _)| s.line.0 >= top).unwrap_or(0))
            };
            self.search = Some(SearchState {
                pattern: pattern.to_string(),
                matches,
                idx,
            });
        } else if let Some(st) = self.search.as_mut() {
            // Same pattern: step the selection (wrapping), no re-scan.
            let n = st.matches.len();
            if n > 0 {
                st.idx = Some(match st.idx {
                    Some(i) if forward => (i + 1) % n,
                    Some(i) => (i + n - 1) % n,
                    None if forward => 0,
                    None => n - 1,
                });
            }
        }

        // Copy out before scrolling (which borrows self mutably).
        let (start, end, idx, total) = {
            let st = self.search.as_ref()?;
            let idx = st.idx?;
            (
                st.matches[idx].0,
                st.matches[idx].1,
                idx,
                st.matches.len() as u32,
            )
        };
        // Bring the match into view: history match -> viewport top; a screen
        // match means offset 0 (already visible).
        let offset = (-start.line.0).clamp(0, hist as i32);
        self.scroll(ScrollAmount::To(offset.min(u16::MAX as i32) as u16));
        let abs = |p: Point| {
            (
                (hist as i64 + p.line.0 as i64).max(0) as u32,
                p.column.0 as u16,
            )
        };
        Some(ember_core::SearchHit {
            start: abs(start),
            end: abs(end),
            ordinal: idx as u32 + 1,
            total,
        })
    }

    /// Enumerate every match for `regex` in buffer order (top of history down),
    /// capped at [`SEARCH_MATCH_CAP`]. Stops when a scan wraps or stalls.
    fn enumerate_matches(
        &self,
        regex: &mut RegexSearch,
        last_col: usize,
        screen: i32,
        hist: usize,
    ) -> Vec<(Point, Point)> {
        let mut out: Vec<(Point, Point)> = Vec::new();
        let mut origin = Point::new(Line(-(hist as i32)), Column(0));
        loop {
            let m = match self
                .term
                .search_next(regex, origin, Direction::Right, Side::Left, None)
            {
                Some(m) => m,
                None => break,
            };
            let (s, e) = (*m.start(), *m.end());
            // Matches come out strictly increasing; anything not past the last
            // means the scan wrapped the buffer or stalled at the end.
            if let Some((ls, _)) = out.last() {
                if (s.line.0, s.column.0) <= (ls.line.0, ls.column.0) {
                    break;
                }
            }
            out.push((s, e));
            if out.len() >= SEARCH_MATCH_CAP {
                break;
            }
            origin = step_cell(s, 1, last_col, screen, hist);
        }
        out
    }
    /// Scroll the display through scrollback history. **No-op on the alternate
    /// screen** (vim/less/htop have no scrollback) — the classic scrollback bug is
    /// scrolling primary history while a full-screen app is up, so we gate it at the
    /// source. `scroll_display` marks the viewport fully damaged on any change, so
    /// the next drain repaints the scrolled view.
    pub fn scroll(&mut self, amount: ScrollAmount) {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let scroll = match amount {
            ScrollAmount::Lines(n) => Scroll::Delta(n),
            ScrollAmount::To(n) => {
                // Absolute offset → delta from the current position.
                let cur = self.term.grid().display_offset() as i32;
                Scroll::Delta(n as i32 - cur)
            }
            ScrollAmount::PageUp => Scroll::PageUp,
            ScrollAmount::PageDown => Scroll::PageDown,
            ScrollAmount::Top => Scroll::Top,
            ScrollAmount::Bottom => Scroll::Bottom,
            _ => return,
        };
        self.term.scroll_display(scroll);
    }

    /// Resize the engine grid.
    pub fn resize(&mut self, dims: GridDims) {
        self.dims = dims;
        self.term.resize(TermSize::new(
            dims.columns as usize,
            dims.screen_lines as usize,
        ));
    }

    pub fn dims(&self) -> GridDims {
        self.dims
    }

    /// Whether the app enabled focus reporting (DEC 1004) — gates writing
    /// `CSI I`/`CSI O` to the PTY on focus changes.
    pub fn reports_focus(&self) -> bool {
        self.term.mode().contains(TermMode::FOCUS_IN_OUT)
    }

    /// Make the next drain a full reset carrying the complete style table —
    /// for a consumer that lost its accumulated grid/style state.
    pub fn request_full(&mut self) {
        self.resync = true;
    }

    fn cursor_state(&self) -> CursorState {
        let point = self.term.grid().cursor.point;
        // The cursor lives on the live screen; scrolled into history it moves
        // down out of the viewport (its viewport row is line + offset).
        let row = point.line.0.max(0) as u32 + self.display_offset as u32;
        let on_screen = row < self.dims.screen_lines as u32;
        let visible = self.term.mode().contains(TermMode::SHOW_CURSOR) && on_screen;
        let shape = match self.term.cursor_style().shape {
            AlacCursorShape::Block | AlacCursorShape::HollowBlock => CursorShape::Block,
            AlacCursorShape::Underline => CursorShape::Underline,
            AlacCursorShape::Beam => CursorShape::Beam,
            AlacCursorShape::Hidden => CursorShape::Hidden,
        };
        CursorState {
            row: row.min(u16::MAX as u32) as u16,
            col: point.column.0 as u16,
            shape,
            visible,
        }
    }
}

impl<L: EventListener> VtProjection for AlacrittyProjection<L> {
    fn drain_damage_into(&mut self, out: &mut GridDelta) {
        self.epoch += 1;
        out.epoch = self.epoch;
        out.dims = self.dims;

        // Re-sync the display offset every drain: it changes both via scroll commands
        // and as output rotates history while scrolled up (the engine bumps it to
        // keep the viewport on the same content — the streaming-scroll fix).
        self.display_offset = self.term.grid().display_offset();

        let cols = self.dims.columns as usize;
        let lines = self.dims.screen_lines as usize;

        // Collect damaged ranges first — the damage iterator borrows the term,
        // and we need to read the grid afterwards. The arm consumes the iterator
        // into an owned Vec, ending the borrow before we read cells.
        let (full, ranges): (bool, Vec<(usize, usize, usize)>) = match self.term.damage() {
            TermDamage::Full => (true, Vec::new()),
            TermDamage::Partial(it) => (false, it.map(|ld| (ld.line, ld.left, ld.right)).collect()),
        };
        self.term.reset_damage();
        let full = full || std::mem::take(&mut self.resync);

        let mut first_seen = Vec::new();
        if full {
            out.reset = true;
            out.cells.clear();
            for line in 0..lines {
                for col in 0..cols {
                    out.cells.push(self.patch(line, col, &mut first_seen));
                }
            }
        } else {
            for (line, left, right) in ranges {
                for col in left..=right {
                    out.cells.push(self.patch(line, col, &mut first_seen));
                }
            }
        }
        // MERGE into whatever the caller left pending (the trait contract) —
        // assigning here would clobber styles from a superseded drain and
        // re-open the black-on-black coalescing bug fixed in GridDelta::merge.
        out.new_styles.extend(first_seen);
        if out.reset {
            // A resync consumer has no style cache at all: ship the full table.
            out.new_styles = self.interner.all();
        }
        out.cursor = self.cursor_state();
        let mode = self.term.mode();
        out.bracketed_paste = mode.contains(TermMode::BRACKETED_PASTE);
        out.display_offset = self.display_offset.min(u16::MAX as usize) as u16;
        out.history_len = self.term.grid().history_size().min(u16::MAX as usize) as u16;
        out.alt_screen = mode.contains(TermMode::ALT_SCREEN);
        // Any mouse-report mode means the app wants the wheel as mouse events.
        out.mouse_reporting = mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
        );
        out.mouse = ember_core::MouseProto {
            click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            drag: mode.contains(TermMode::MOUSE_DRAG),
            motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
        };
        // OSC 133 gutter marks visible in the current viewport. The alt screen has no
        // scrollback (and command marks belong to the primary screen), so none there.
        out.marks.clear();
        if !out.alt_screen {
            let hist = self.term.grid().history_size() as i64;
            let screen = self.dims.screen_lines as i64;
            let bottom_abs = hist + screen - 1;
            // Marks whose prompt line was erased *in place* — `clear`/Ctrl+L emit ED
            // (`ESC [ J`) without scrolling to history, so the cells go blank but the
            // mark, pinned to an absolute line, would otherwise keep painting on the
            // now-empty row. Detect the blanked line and drop the stale mark for good.
            let mut stale: Vec<i64> = Vec::new();
            for m in &self.marks {
                // visible row r: abs = bottom_abs - (screen-1) + r - display_offset
                let r = m.abs_line - bottom_abs + (screen - 1) + self.display_offset as i64;
                if (0..screen).contains(&r) {
                    let engine_line = r as i32 - self.display_offset as i32;
                    if line_is_blank(&self.term, engine_line, cols) {
                        stale.push(m.abs_line);
                        continue;
                    }
                    let status = if m.manual {
                        MarkStatus::Manual
                    } else {
                        match m.exit {
                            None => MarkStatus::Running,
                            Some(0) => MarkStatus::Ok,
                            Some(_) => MarkStatus::Fail,
                        }
                    };
                    out.marks.push((r as u16, status));
                }
            }
            if !stale.is_empty() {
                self.marks.retain(|m| !stale.contains(&m.abs_line));
            }
        }
    }
}

impl<L: EventListener> AlacrittyProjection<L> {
    fn patch(
        &mut self,
        line: usize,
        col: usize,
        first_seen: &mut Vec<(StyleId, Style)>,
    ) -> CellPatch {
        // Map a visible row to the engine's line index accounting for scroll: when
        // scrolled up by `display_offset`, visible row `v` shows engine line
        // `v - display_offset` (history lines are negative). At the bottom
        // (`display_offset == 0`) this is just `Line(v)`.
        let engine_line = line as i32 - self.display_offset as i32;
        let (content, style, wrapped, wide) = {
            let cell = &self.term.grid()[Point::new(Line(engine_line), Column(col))];
            neutral_of(cell, &self.palette, self.term.colors())
        };
        let id = self.interner.intern(style, first_seen);
        CellPatch {
            row: line as u16,
            col: col as u16,
            cell: NeutralCell {
                content,
                style: id,
                wrapped,
                wide,
            },
        }
    }
}

/// Resolve one alacritty cell into neutral content + style + soft-wrap + wide.
/// `colors` overlays runtime OSC 4/10/11 palette changes over our defaults.
/// Whether an engine line (`0..screen` = visible, negative = history) holds only
/// blank cells — used to detect a prompt line erased in place by ED (`clear` /
/// Ctrl+L), so its now-stale OSC 133 gutter mark can be dropped. A real prompt
/// line always carries at least the prompt string, so all-spaces == erased.
fn line_is_blank<L: EventListener>(term: &Term<L>, line: i32, cols: usize) -> bool {
    let grid = term.grid();
    (0..cols).all(|col| grid[Point::new(Line(line), Column(col))].c == ' ')
}

fn neutral_of(
    cell: &Cell,
    palette: &Palette,
    colors: &alacritty_terminal::term::color::Colors,
) -> (CellContent, Style, bool, bool) {
    let flags = cell.flags;
    let mut fg = palette.resolve_over(colors, cell.fg);
    let mut bg = palette.resolve_over(colors, cell.bg);
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }

    let mut attrs = Attrs::empty();
    if flags.contains(Flags::BOLD) {
        attrs |= Attrs::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        attrs |= Attrs::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        attrs |= Attrs::UNDERLINE;
    }
    if flags.contains(Flags::STRIKEOUT) {
        attrs |= Attrs::STRIKEOUT;
    }
    if flags.contains(Flags::DIM) {
        attrs |= Attrs::DIM;
    }
    if flags.contains(Flags::HIDDEN) {
        attrs |= Attrs::HIDDEN;
    }

    let style = Style { fg, bg, attrs };
    // Wide (2-column) glyphs: the leader carries `wide`; the following spacer
    // cell ships as the self-describing `WideSpacer` (B1 ruling :
    // per-cell damage can split the pair, so the spacer must stand alone).
    let wide = flags.intersects(Flags::WIDE_CHAR);
    let content = if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
        CellContent::WideSpacer
    } else {
        // Combining marks / ZWJ tails live in the cell's zerowidth storage —
        // fold them into a Cluster so NFD accents survive the seam.
        match (cell.c, cell.zerowidth()) {
            (c, Some(zw)) if !zw.is_empty() => {
                let mut s = String::with_capacity(1 + zw.len() * 4);
                s.push(c);
                s.extend(zw);
                CellContent::Cluster(s.into_boxed_str())
            }
            (' ' | '\0', _) => CellContent::Empty,
            (c, _) => CellContent::Char(c),
        }
    };
    let wide = wide && !matches!(content, CellContent::WideSpacer | CellContent::Empty);
    // Last cell of a soft-wrapped row carries WRAPLINE — the logical line continues.
    let wrapped = flags.contains(Flags::WRAPLINE);
    (content, style, wrapped, wide)
}

/// Step one cell right (`+1`) or left (`-1`) from `p`, wrapping across line
/// ends, clamped to the buffer (`-hist..screen`): the "advance past the last
/// match" origin for continued searches.
fn step_cell(p: Point, dir: i8, last_col: usize, screen: i32, hist: usize) -> Point {
    if dir > 0 {
        if p.column.0 < last_col {
            Point::new(p.line, Column(p.column.0 + 1))
        } else if p.line.0 < screen - 1 {
            Point::new(Line(p.line.0 + 1), Column(0))
        } else {
            p // buffer bottom-right: stay (wrap-around search still works)
        }
    } else if p.column.0 > 0 {
        Point::new(p.line, Column(p.column.0 - 1))
    } else if p.line.0 > -(hist as i32) {
        Point::new(Line(p.line.0 - 1), Column(last_col))
    } else {
        p // top of history: stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;

    fn proj() -> AlacrittyProjection<VoidListener> {
        AlacrittyProjection::new(GridDims::new(80, 24), VoidListener)
    }

    fn find(d: &GridDelta, row: u16, col: u16) -> &CellPatch {
        d.cells
            .iter()
            .find(|p| p.row == row && p.col == col)
            .expect("cell present")
    }

    #[test]
    fn osc133_marks_survive_read_boundaries() {
        // Split every which way: mid-prefix, mid-params, and mid-ST.
        let mut p = proj();
        let mut ev = Vec::new();
        ev.extend(p.advance(b"prompt \x1b]1"));
        ev.extend(p.advance(b"33;A\x07 cmd \x1b]133;D;"));
        ev.extend(p.advance(b"13\x1b"));
        ev.extend(p.advance(b"\\ tail"));
        assert_eq!(
            ev,
            vec![OscEvent::PromptStart, OscEvent::CommandEnd(Some(13))]
        );
        // The held-back tail must not leak into later scans.
        assert_eq!(p.advance(b"plain output"), vec![]);
    }

    #[test]
    fn osc1337_current_dir_and_remote_host_are_emitted() {
        let mut p = proj();
        let ev = p.advance(
            b"\x1b]1337;CurrentDir=/home/user/projects\x07\x1b]1337;RemoteHost=user@host\x07",
        );
        assert_eq!(
            ev,
            vec![
                OscEvent::CurrentDir("/home/user/projects".to_string()),
                OscEvent::RemoteHost("user@host".to_string()),
            ]
        );
    }

    #[test]
    fn osc1337_marks_survive_read_boundaries() {
        // Mirrors `osc133_marks_survive_read_boundaries`, split mid-key.
        let mut p = proj();
        let mut ev = Vec::new();
        ev.extend(p.advance(b"$ \x1b]13"));
        ev.extend(p.advance(b"37;CurrentDir=/tm"));
        ev.extend(p.advance(b"p\x07 tail"));
        assert_eq!(ev, vec![OscEvent::CurrentDir("/tmp".to_string())]);
        assert_eq!(p.advance(b"plain output"), vec![]);
    }

    #[test]
    fn set_mark_adds_a_manual_gutter_mark() {
        let mut p = proj();
        p.advance(b"line one\r\n\x1b]1337;SetMark\x07line two\r\n");
        let mut d = GridDelta::default();
        p.drain_damage_into(&mut d);
        assert_eq!(
            d.marks.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
            vec![MarkStatus::Manual]
        );
    }

    #[test]
    fn osc133_and_osc1337_in_the_same_read_stay_in_buffer_order() {
        // CurrentDir before the prompt, SetMark after — order must survive the
        // merge of the two independently-scanned protocols.
        let mut p = proj();
        let ev = p.advance(b"\x1b]1337;CurrentDir=/tmp\x07\x1b]133;A\x07$ \x1b]1337;SetMark\x07");
        assert_eq!(
            ev,
            vec![
                OscEvent::CurrentDir("/tmp".to_string()),
                OscEvent::PromptStart,
                OscEvent::SetMark,
            ]
        );
    }

    #[test]
    fn request_full_reships_reset_plus_complete_style_table() {
        let mut p = proj();
        p.advance(b"\x1b[31mred\x1b[0m plain");
        let mut d1 = GridDelta::default();
        p.drain_damage_into(&mut d1); // consumer learned the styles here
        assert!(!d1.new_styles.is_empty());

        // Steady state: nothing new.
        p.advance(b"x");
        let mut d2 = GridDelta::default();
        p.drain_damage_into(&mut d2);

        // A fresh consumer asks for everything.
        p.request_full();
        let mut d3 = GridDelta::default();
        p.drain_damage_into(&mut d3);
        assert!(d3.reset, "resync must be a full reset");
        assert_eq!(
            d3.new_styles.len(),
            d1.new_styles.len() + d2.new_styles.len(),
            "resync must carry the COMPLETE style table"
        );
        assert!(!d3.cells.is_empty());
    }

    #[test]
    fn drain_merges_into_pending_styles_instead_of_clobbering() {
        let mut p = proj();
        p.advance(b"hi");
        let mut d = GridDelta::default();
        // Simulate a pending style from a superseded (non-reset) drain.
        let sentinel = (StyleId(9999), Style::default());
        d.new_styles.push(sentinel);
        p.drain_damage_into(&mut d);
        if !d.reset {
            assert!(
                d.new_styles.contains(&sentinel),
                "non-reset drain must not clobber pending styles"
            );
        }
    }

    #[test]
    fn wide_and_combining_cells_cross_the_seam() {
        let mut p = proj();
        p.advance("漢e\u{0301}".as_bytes());
        let mut d = GridDelta::default();
        p.drain_damage_into(&mut d);
        let leader = find(&d, 0, 0);
        assert_eq!(leader.cell.content, CellContent::Char('漢'));
        assert!(leader.cell.wide, "CJK leader must carry wide");
        let spacer = find(&d, 0, 1);
        assert_eq!(spacer.cell.content, CellContent::WideSpacer);
        assert!(!spacer.cell.wide);
        let combining = find(&d, 0, 2);
        assert_eq!(
            combining.cell.content,
            CellContent::Cluster("e\u{0301}".into()),
            "NFD accent must fold into a Cluster"
        );
        assert!(!combining.cell.wide);
    }

    #[test]
    fn projects_typed_bytes_into_cells() {
        let mut p = proj();
        p.advance(b"hi");
        let mut delta = GridDelta::default();
        p.drain_damage_into(&mut delta);
        assert!(delta.reset, "first drain is a full reset");
        assert_eq!(find(&delta, 0, 0).cell.content, CellContent::Char('h'));
        assert_eq!(find(&delta, 0, 1).cell.content, CellContent::Char('i'));
        assert_eq!(delta.dims, GridDims::new(80, 24));
        assert_eq!(delta.epoch, 1);
    }

    #[test]
    fn newline_moves_to_next_row() {
        let mut p = proj();
        p.advance(b"a\r\nb");
        let mut delta = GridDelta::default();
        p.drain_damage_into(&mut delta);
        assert_eq!(find(&delta, 0, 0).cell.content, CellContent::Char('a'));
        assert_eq!(find(&delta, 1, 0).cell.content, CellContent::Char('b'));
    }

    #[test]
    fn background_color_resolves_to_palette_not_black() {
        //  guard: a cell with an ANSI background (SGR 44 = blue bg) must
        // resolve to the palette color, never the default/pure-black that a
        // missing style would produce. (The black-block symptom was actually the
        // styles-dropped-on-reset coalescing bug, fixed in ember-core::merge.)
        use ember_core::Rgb;
        let mut p = proj();
        p.advance(b"\x1b[44mX");
        let mut d = GridDelta::default();
        p.drain_damage_into(&mut d);
        let cell = find(&d, 0, 0);
        let style = d
            .new_styles
            .iter()
            .find(|(id, _)| *id == cell.cell.style)
            .map(|(_, s)| *s)
            .expect("style shipped in new_styles");
        assert_eq!(cell.cell.content, CellContent::Char('X'));
        assert_eq!(
            style.bg,
            Rgb::new(0x3b, 0x8e, 0xea),
            "blue bg from the palette"
        );
        assert_ne!(style.bg, Rgb::new(0, 0, 0), "never pure black");
    }

    #[test]
    fn styles_are_interned_and_shipped() {
        let mut p = proj();
        p.advance(b"x");
        let mut delta = GridDelta::default();
        p.drain_damage_into(&mut delta);
        // The 'x' cell references a style id that appears in new_styles.
        let x = find(&delta, 0, 0);
        assert!(delta.new_styles.iter().any(|(id, _)| *id == x.cell.style));
    }

    #[test]
    fn second_drain_after_reset_is_partial() {
        let mut p = proj();
        p.advance(b"a");
        let mut first = GridDelta::default();
        p.drain_damage_into(&mut first);
        assert!(first.reset);
        // Type more; the next drain should be partial (not a full reset).
        p.advance(b"b");
        let mut second = GridDelta::default();
        p.drain_damage_into(&mut second);
        assert!(!second.reset, "incremental drain is not a full reset");
        assert_eq!(find(&second, 0, 1).cell.content, CellContent::Char('b'));
    }

    fn feed_lines(p: &mut AlacrittyProjection<VoidListener>, n: usize) {
        let mut s = String::new();
        for i in 1..=n {
            s.push_str(&format!("L{i}\r\n"));
        }
        p.advance(s.as_bytes());
    }

    #[test]
    fn search_finds_scrollback_text_and_scrolls_to_it() {
        let mut p = proj();
        p.advance(b"needle-here\r\n");
        feed_lines(&mut p, 60); // push the needle deep into history
        let hit = p.search("needle-here", true).expect("found");
        // Line 1 of the session: absolute line 1 (prompt-less test feed).
        assert_eq!(hit.start.0, hit.end.0, "single-line match");
        assert_eq!(hit.start.1, 0, "starts at column 0");
        assert_eq!(hit.end.1 - hit.start.1, 10, "11 columns wide");
        let mut d = GridDelta::default();
        p.drain_damage_into(&mut d);
        assert!(
            d.display_offset > 0,
            "display scrolled up to show the match"
        );
        // The absolute position projects into the now-scrolled viewport.
        let row0_abs = d.history_len as u32 - d.display_offset as u32;
        assert!(hit.start.0 >= row0_abs && hit.start.0 < row0_abs + 24);
    }

    #[test]
    fn search_next_advances_between_matches_and_wraps() {
        let mut p = proj();
        p.advance(b"target one\r\n");
        feed_lines(&mut p, 10);
        p.advance(b"target two\r\n");
        feed_lines(&mut p, 10);
        let first = p.search("target", true).expect("first");
        let second = p.search("target", true).expect("second");
        assert_ne!(first.start, second.start, "advanced to the other match");
        let third = p.search("target", true).expect("wrapped");
        assert_eq!(third.start, first.start, "wrapped back around");
        let back = p.search("target", false).expect("prev");
        assert_eq!(back.start, second.start, "prev goes back");
    }

    #[test]
    fn search_reports_ordinal_and_total_and_advances() {
        let mut p = proj();
        for _ in 0..3 {
            p.advance(b"target\r\n");
        }
        feed_lines(&mut p, 5); // push all 3 up into history
        let a = p.search("target", true).expect("found");
        assert_eq!(a.total, 3, "counts every match");
        assert_eq!(a.ordinal, 1, "starts on the first (all are above the view)");
        let b = p.search("target", true).expect("next");
        assert_eq!((b.total, b.ordinal), (3, 2), "next advances the ordinal");
        let c = p.search("target", false).expect("prev");
        assert_eq!(c.ordinal, 1, "prev retreats");
        let none = p.search("no-such-text-anywhere", true);
        assert!(none.is_none(), "a miss reports no hit");
    }

    #[test]
    fn search_smart_case_and_invalid_patterns() {
        let mut p = proj();
        p.advance(b"MixedCase word\r\n");
        feed_lines(&mut p, 5);
        assert!(
            p.search("mixedcase", true).is_some(),
            "lowercase pattern matches case-insensitively"
        );
        assert!(
            p.search("MIXEDCASE", true).is_none(),
            "uppercase pattern is exact"
        );
        assert!(
            p.search("[invalid(", true).is_none(),
            "bad regex = no match"
        );
        assert!(p.search("", true).is_none(), "empty clears");
    }

    #[test]
    fn scrolls_primary_history() {
        let mut p = proj();
        feed_lines(&mut p, 60); // 60 lines → history above the 24-row screen
        p.scroll(ScrollAmount::Lines(5));
        let mut d = GridDelta::default();
        p.drain_damage_into(&mut d);
        assert!(!d.alt_screen);
        assert!(d.history_len > 0, "history should exist");
        assert!(d.display_offset > 0, "should be scrolled up into history");
    }

    #[test]
    fn scroll_is_noop_on_alt_screen() {
        let mut p = proj();
        feed_lines(&mut p, 60);
        p.advance(b"\x1b[?1049h"); // enter the alternate screen (vim/less/htop)
        p.scroll(ScrollAmount::PageUp); // must NOT touch primary history
        let mut d = GridDelta::default();
        p.drain_damage_into(&mut d);
        assert!(d.alt_screen);
        assert_eq!(
            d.display_offset, 0,
            "the alt screen has no scrollback — scrolling must be a no-op"
        );
    }
}
