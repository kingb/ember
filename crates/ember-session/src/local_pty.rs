//! `LocalPty` — the v1 [`SessionBackend`] (design §4; ).
//!
//! Spawns a shell in a real PTY (`portable-pty`) and runs a dedicated emulation
//! thread that owns the [`AlacrittyProjection`]. PTY output is parsed into the
//! engine and drained into owned [`GridDelta`]s on the pixel lane; engine query
//! responses (`PtyWrite`) are routed back to the PTY; title/bell/exit surface on
//! the semantic lane. The trait never exposes the fd — zero PTY-ness holds.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use ember_core::{
    BackendControl, BackendEvent, BackendHandle, ExitStatus, FrameTx, GridDelta, GridDims,
    ScrollAmount, SessionBackend, SessionId, VtProjection, frame_channel,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::projection::AlacrittyProjection;

/// How to launch a local session.
#[derive(Clone, Debug)]
pub struct LocalPtyConfig {
    pub id: SessionId,
    pub dims: GridDims,
    /// Program to run; `None` → `$SHELL`, falling back to `/bin/sh`.
    pub program: Option<String>,
    pub args: Vec<String>,
    /// Working directory; `None` → `$HOME`.
    pub cwd: Option<PathBuf>,
    /// Auto-inject OSC 133 shell integration (zsh/bash) so the exit-status gutter
    /// and jump-to-prompt work without the user editing their rc. Chains the user's
    /// config, never replaces it.
    pub shell_integration: bool,
}

impl LocalPtyConfig {
    pub fn new(id: SessionId, dims: GridDims) -> Self {
        Self {
            id,
            dims,
            program: None,
            args: Vec::new(),
            cwd: None,
            shell_integration: true,
        }
    }
}

/// The v1 local-PTY backend.
pub struct LocalPty;

impl SessionBackend for LocalPty {
    type Config = LocalPtyConfig;

    fn spawn(config: Self::Config) -> std::io::Result<BackendHandle> {
        let LocalPtyConfig {
            id,
            dims,
            program,
            args,
            cwd,
            shell_integration,
        } = config;

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: dims.screen_lines,
                cols: dims.columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(std::io::Error::other)?;

        let program = program
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(program.clone());
        // Auto-inject OSC 133 shell integration (env + rcfile args), chaining the
        // user's own config. Best-effort: unsupported shells / IO errors → no-op.
        if shell_integration {
            let inj = crate::shell_integration::prepare(
                &program,
                &crate::shell_integration::integration_dir(),
            );
            for a in inj.args {
                cmd.arg(a);
            }
            for (k, v) in inj.env {
                cmd.env(k, v);
            }
        }
        for a in args {
            cmd.arg(a);
        }
        cmd.env("TERM", "xterm-256color");
        match cwd {
            Some(dir) => cmd.cwd(dir),
            None => {
                if let Some(home) = std::env::var_os("HOME") {
                    cmd.cwd(home);
                }
            }
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        drop(pair.slave); // let the reader hit EOF when the child exits (macOS-correct)

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;
        let writer = pair.master.take_writer().map_err(std::io::Error::other)?;
        let master = pair.master;

        let (ctrl_tx, ctrl_rx) = mpsc::channel::<BackendControl>();
        let (event_tx, event_rx) = mpsc::channel::<BackendEvent>();
        let (frame_tx, frame_rx) = frame_channel();
        let (itx, irx) = mpsc::channel::<Ev>();

        // Reader thread: PTY bytes → internal channel.
        {
            let itx = itx.clone();
            thread::spawn(move || reader_loop(reader, itx));
        }
        // Forwarder thread: external control → internal channel (so the
        // emulation thread blocks on a single receiver).
        {
            let itx = itx.clone();
            thread::spawn(move || {
                for msg in ctrl_rx {
                    if itx.send(Ev::Control(msg)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(itx);

        // Emulation thread: owns the engine + PTY write side.
        {
            let event_tx = event_tx.clone();
            thread::spawn(move || {
                emulation_loop(dims, event_tx, frame_tx, irx, writer, master, child)
            });
        }

        Ok(BackendHandle {
            id,
            control: ctrl_tx,
            frames: frame_rx,
            events: event_rx,
        })
    }
}

/// Internal event funnel for the emulation thread.
enum Ev {
    Pty(Vec<u8>),
    Control(BackendControl),
    PtyClosed,
}

/// Forwards alacritty engine events onto the semantic lane and routes engine
/// query responses (`PtyWrite`) into a shared outbox the emulation thread drains.
struct EmberListener {
    events: Sender<BackendEvent>,
    outbox: Arc<Mutex<Vec<u8>>>,
}

impl EventListener for EmberListener {
    fn send_event(&self, event: AlacEvent) {
        match event {
            AlacEvent::Title(title) => {
                let _ = self.events.send(BackendEvent::Title(title));
            }
            AlacEvent::Bell => {
                let _ = self.events.send(BackendEvent::Bell);
            }
            AlacEvent::PtyWrite(text) => {
                self.outbox
                    .lock()
                    .unwrap()
                    .extend_from_slice(text.as_bytes());
            }
            _ => {}
        }
    }
}

fn reader_loop(mut reader: Box<dyn Read + Send>, itx: Sender<Ev>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = itx.send(Ev::PtyClosed);
                break;
            }
            Ok(n) => {
                if itx.send(Ev::Pty(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
}

fn emulation_loop(
    dims: GridDims,
    event_tx: Sender<BackendEvent>,
    frame_tx: FrameTx,
    irx: mpsc::Receiver<Ev>,
    mut writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) {
    let outbox = Arc::new(Mutex::new(Vec::<u8>::new()));
    let listener = EmberListener {
        events: event_tx.clone(),
        outbox: Arc::clone(&outbox),
    };
    let mut proj = AlacrittyProjection::new(dims, listener);
    push_frame(&mut proj, &frame_tx);

    for ev in irx {
        match ev {
            Ev::Pty(bytes) => {
                for osc in proj.advance(&bytes) {
                    let _ = event_tx.send(BackendEvent::Osc(osc));
                }
                flush_outbox(&outbox, &mut writer);
                push_frame(&mut proj, &frame_tx);
            }
            Ev::Control(BackendControl::Input(bytes)) => {
                // Typing snaps the view back to the live bottom (standard terminal
                // behavior). No-op + no redraw if already there.
                proj.scroll(ScrollAmount::Bottom);
                push_frame(&mut proj, &frame_tx);
                let _ = writer.write_all(&bytes);
                let _ = writer.flush();
            }
            Ev::Control(BackendControl::Scroll(amount)) => {
                proj.scroll(amount);
                push_frame(&mut proj, &frame_tx);
            }
            Ev::Control(BackendControl::JumpMark(dir)) => {
                proj.scroll_to_prompt(dir);
                push_frame(&mut proj, &frame_tx);
            }
            Ev::Control(BackendControl::Resize(new_dims)) => {
                let _ = master.resize(PtySize {
                    rows: new_dims.screen_lines,
                    cols: new_dims.columns,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                proj.resize(new_dims);
                push_frame(&mut proj, &frame_tx);
            }
            Ev::Control(BackendControl::Focus(_)) => {}
            Ev::Control(BackendControl::Shutdown) => {
                let _ = child.kill();
                break;
            }
            Ev::Control(_) => {}
            Ev::PtyClosed => {
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                let _ = event_tx.send(BackendEvent::Exited(ExitStatus { code }));
                break;
            }
        }
    }
}

fn push_frame(proj: &mut AlacrittyProjection<EmberListener>, frame_tx: &FrameTx) {
    let mut delta = GridDelta::default();
    proj.drain_damage_into(&mut delta);
    if delta.reset || !delta.cells.is_empty() {
        frame_tx.push(delta);
    }
}

fn flush_outbox(outbox: &Arc<Mutex<Vec<u8>>>, writer: &mut Box<dyn Write + Send>) {
    let mut buf = outbox.lock().unwrap();
    if !buf.is_empty() {
        let _ = writer.write_all(&buf);
        let _ = writer.flush();
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_core::CellContent;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// Reconstruct row 0's text from the frame lane until `needle` appears or we
    /// time out. Proves the full path: shell → PTY → engine → projection → lane.
    #[test]
    fn live_shell_output_reaches_the_grid() {
        let mut cfg = LocalPtyConfig::new(SessionId::new("test"), GridDims::new(80, 24));
        cfg.program = Some("/bin/sh".to_string());
        cfg.args = vec!["-c".to_string(), "printf 'hello-ember'".to_string()];

        let handle = LocalPty::spawn(cfg).expect("spawn LocalPty");

        let mut grid: HashMap<(u16, u16), char> = HashMap::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut row0 = String::new();
        let mut found = false;
        while Instant::now() < deadline {
            while let Some(delta) = handle.frames.take() {
                for patch in delta.cells {
                    let ch = match patch.cell.content {
                        CellContent::Char(c) => c,
                        _ => ' ',
                    };
                    grid.insert((patch.row, patch.col), ch);
                }
            }
            row0 = (0..80)
                .map(|c| *grid.get(&(0, c)).unwrap_or(&' '))
                .collect();
            if row0.contains("hello-ember") {
                found = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(found, "expected 'hello-ember' in row 0, got {row0:?}");
    }

    #[test]
    fn input_is_echoed_back_to_the_grid() {
        let cfg = LocalPtyConfig::new(SessionId::new("echo"), GridDims::new(80, 24));
        let handle = LocalPty::spawn(cfg).expect("spawn LocalPty");

        // Drive an interactive shell: type a command that prints a sentinel.
        handle
            .control
            .send(BackendControl::Input(
                b"printf 'XYZZY-mark'\n".to_vec().into_boxed_slice(),
            ))
            .expect("send input");

        let mut grid: HashMap<(u16, u16), char> = HashMap::new();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut found = false;
        while Instant::now() < deadline && !found {
            while let Some(delta) = handle.frames.take() {
                for patch in delta.cells {
                    if let CellContent::Char(c) = patch.cell.content {
                        grid.insert((patch.row, patch.col), c);
                    }
                }
            }
            // Scan the whole grid for the sentinel on any row.
            for row in 0..24u16 {
                let line: String = (0..80)
                    .map(|c| *grid.get(&(row, c)).unwrap_or(&' '))
                    .collect();
                if line.contains("XYZZY-mark") {
                    found = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = handle.control.send(BackendControl::Shutdown);
        assert!(found, "sentinel never reached the grid");
    }
}
