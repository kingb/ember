//! The `SessionBackend` contract (design §4; the B1 contract signature 2).
//!
//! A backend is "a thing that runs a session and produces a renderable neutral
//! grid." Its defining invariant is **zero PTY-ness**: nothing here exposes a
//! file descriptor. It is bytes-in (control), owned-events-out (two lanes) —
//! never grid-borrow-out. Each backend runs a dedicated emulation thread per
//! pane that owns the VT engine; the projection's owned `Send` delta is the
//! only thing that crosses the thread boundary.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::grid::{GridDelta, GridDims};
use crate::ids::SessionId;

/// The projection: drain the engine's accumulated damage into a render-bound
/// delta, then clear the engine's native damage (the B1 contract signature 1). The
/// implementor owns the VT engine and runs on its thread; `alacritty`-v1 and
/// `libghostty`-phase-2 differ only in which `VtProjection` is compiled.
pub trait VtProjection {
    /// Drain accumulated damage into `out`, merging into whatever is already
    /// pending, then clear the engine's native damage. `out` is the
    /// render-bound delta.
    fn drain_damage_into(&mut self, out: &mut GridDelta);
}

/// Inbound control — the "command-in" face (`BackendControl`; design §4
/// calls this `SessionCommand`). Data-only + serde so it serializes unchanged
/// onto the backend bus later; `#[non_exhaustive]` for additive evolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BackendControl {
    /// Bytes to write to the session (keyboard input, paste, …).
    Input(Box<[u8]>),
    /// Resize the session to a new grid.
    Resize(GridDims),
    /// Focus gained/lost (drives focus-reporting + cursor blink).
    Focus(bool),
    /// Scroll the display through scrollback history (engine-agnostic).
    Scroll(ScrollAmount),
    /// Jump the viewport to the previous (`-1`) / next (`+1`) OSC 133 prompt mark.
    JumpMark(i8),
    /// Tear the session down.
    Shutdown,
}

/// A scrollback movement, in engine-neutral terms. `Lines(+n)` scrolls **up**
/// into history, `Lines(-n)` scrolls back down toward the live bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ScrollAmount {
    /// Scroll by `n` lines: positive = up (into history), negative = down.
    Lines(i32),
    /// Jump to an absolute display offset (lines up from the bottom) — for the
    /// scrollbar thumb drag.
    To(u16),
    /// Up one screenful.
    PageUp,
    /// Down one screenful.
    PageDown,
    /// Jump to the oldest history line.
    Top,
    /// Jump to the live bottom.
    Bottom,
}

/// Shell-integration / OSC semantic events (design §8.1: OSC 133 backbone + an
/// iTerm2 OSC 1337 subset). Rides the ordered semantic lane.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OscEvent {
    // OSC 133 (FinalTerm) semantic marks.
    PromptStart,
    CommandStart,
    OutputStart,
    CommandEnd(Option<i32>),
    // iTerm2 OSC 1337 subset.
    CurrentDir(String),
    RemoteHost(String),
    SetMark,
}

/// Clipboard request from the session (OSC 52). The *policy* lives in
/// `ember-core`; the actual read/write is a `PlatformBackend` effect (design §7).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClipboardOp {
    Set(String),
    RequestPaste,
}

/// Opaque phase-2 passthrough payload (libghostty-vt: Kitty graphics, `tmux -CC`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassthroughEvent(pub Vec<u8>);

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ExitStatus {
    pub code: Option<i32>,
}

/// Semantic-lane event — the "events-out" face (`BackendEvent`). This
/// lane is **ordered + reliable**: events must not be dropped or reordered.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendEvent {
    Title(String),
    Bell,
    Osc(OscEvent),
    Clipboard(ClipboardOp),
    /// Phase-2 (libghostty-vt) passthrough: Kitty graphics, `tmux -CC`.
    Passthrough(PassthroughEvent),
    Exited(ExitStatus),
}

/// The shared single slot behind the pixel lane.
#[derive(Default, Debug)]
struct FrameSlot {
    pending: Mutex<Option<GridDelta>>,
}

/// Producer end of the pixel lane (held by the emulation thread).
#[derive(Clone, Debug)]
pub struct FrameTx {
    slot: Arc<FrameSlot>,
}

/// Consumer end of the pixel lane (held by render).
#[derive(Debug)]
pub struct FrameRx {
    slot: Arc<FrameSlot>,
}

/// The pixel lane (the B1 contract signature 2, lane 1): a **single-slot, latest-wins,
/// merge-on-overwrite** mailbox for `GridDelta`. If render falls behind, damage
/// coalesces in the pending delta — there is never an unbounded queue of frames.
pub fn frame_channel() -> (FrameTx, FrameRx) {
    let slot = Arc::new(FrameSlot::default());
    (
        FrameTx {
            slot: Arc::clone(&slot),
        },
        FrameRx { slot },
    )
}

impl FrameTx {
    /// Publish a delta. If one is still pending (render is behind), merge into it
    /// so frames coalesce rather than queue.
    pub fn push(&self, delta: GridDelta) {
        let mut slot = self.slot.pending.lock().unwrap();
        match slot.take() {
            Some(mut pending) => {
                pending.merge(delta);
                *slot = Some(pending);
            }
            None => *slot = Some(delta),
        }
    }
}

impl FrameRx {
    /// Take the delta accumulated since the last take, clearing the slot. Returns
    /// `None` when nothing new has been produced.
    pub fn take(&self) -> Option<GridDelta> {
        self.slot.pending.lock().unwrap().take()
    }
}

/// A handle to a running session — its **three faces** (the B1 contract signature 2):
/// command-in (`control`), grid-out (`frames`, the pixel lane), and events-out
/// (`events`, the semantic lane). Carries **no file descriptor** — the zero-PTY
/// guard. The emulation thread lives behind these channels.
#[derive(Debug)]
pub struct BackendHandle {
    pub id: SessionId,
    /// Lane 0 — inbound control (Send).
    pub control: Sender<BackendControl>,
    /// Lane 1 — pixel: latest-wins / coalescing `GridDelta`.
    pub frames: FrameRx,
    /// Lane 2 — semantic: ordered, reliable.
    pub events: Receiver<BackendEvent>,
}

/// A session backend (design §4). Implementors spawn a session on a dedicated
/// emulation thread and hand back a [`BackendHandle`]. The trait exposes **no
/// file descriptor** — the zero-PTY-ness guard, structurally enforced by the
/// fact that nothing in this contract is fd-shaped.
pub trait SessionBackend {
    /// Per-backend spawn configuration (`LocalPty` wants a shell + cwd; a future
    /// `a future out-of-process backend` wants a bus `AgentRef`).
    type Config;

    /// Spawn the session and return its two-lane handle.
    fn spawn(config: Self::Config) -> std::io::Result<BackendHandle>
    where
        Self: Sized;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{CellContent, CellPatch, NeutralCell, StyleId};

    fn one_cell_delta(epoch: u64, ch: char) -> GridDelta {
        let mut d = GridDelta::new(epoch, GridDims::new(80, 24));
        d.cells = vec![CellPatch {
            row: 0,
            col: 0,
            cell: NeutralCell::new(CellContent::Char(ch), StyleId(0)),
        }];
        d
    }

    #[test]
    fn frame_lane_delivers_latest() {
        let (tx, rx) = frame_channel();
        tx.push(one_cell_delta(1, 'a'));
        let got = rx.take().expect("a delta");
        assert_eq!(got.epoch, 1);
        // Slot is now empty.
        assert!(rx.take().is_none());
    }

    #[test]
    fn frame_lane_coalesces_when_consumer_is_behind() {
        let (tx, rx) = frame_channel();
        tx.push(one_cell_delta(1, 'a'));
        tx.push(one_cell_delta(2, 'b')); // render didn't take between pushes
        let got = rx.take().expect("a coalesced delta");
        // Two pushes coalesced into one slot; newer cell at (0,0) wins, epoch 2.
        assert_eq!(got.epoch, 2);
        assert_eq!(got.cells.len(), 1);
        assert_eq!(got.cells[0].cell.content, CellContent::Char('b'));
        assert!(rx.take().is_none());
    }
}
