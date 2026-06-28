//! `ember-term` — the Ember terminal binary (design §2; ).
//!
//! Owns the event loop (winit), wires the window (`ember-platform`) to the GPU
//! renderer (`ember-render`) and a `LocalPty` session (`ember-session`): PTY
//! output flows over the pixel lane into the grid; keystrokes flow back as
//! `BackendControl::Input`. This is the v1 Foundation milestone — a live shell
//! on screen, on Linux and macOS.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ember_core::{
    BackendControl, BackendEvent, BackendHandle, GridDims, SessionBackend, SessionId,
};
use ember_render::{CELL_HEIGHT, CELL_WIDTH, Renderer};
use ember_session::{LocalPty, LocalPtyConfig};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::WindowId;

const PAD: f32 = 4.0;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// How often the loop polls the pixel lane when idle (~125 Hz). A proxy-waker on
/// frame push is the noted refinement; this keeps CPU sane without it.
const POLL: Duration = Duration::from_millis(8);

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        print_banner();
        return;
    }
    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::default();
    event_loop.run_app(&mut app).expect("run event loop");
}

fn print_banner() {
    println!(
        "ember-term {} (core {}, session {}, render {}, platform {})",
        env!("CARGO_PKG_VERSION"),
        ember_core::version(),
        ember_session::core_version(),
        ember_render::core_version(),
        ember_platform::core_version(),
    );
}

#[derive(Default)]
struct App {
    state: Option<RunState>,
}

struct RunState {
    renderer: Renderer,
    session: BackendHandle,
    modifiers: ModifiersState,
    dims: GridDims,
}

/// Compute the cell grid that fits a pixel surface.
fn dims_for(width: u32, height: u32) -> GridDims {
    let cols = (((width as f32 - 2.0 * PAD) / CELL_WIDTH).floor() as i64).clamp(1, u16::MAX as i64);
    let rows =
        (((height as f32 - 2.0 * PAD) / CELL_HEIGHT).floor() as i64).clamp(1, u16::MAX as i64);
    GridDims::new(cols as u16, rows as u16)
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let w = DEFAULT_COLS as f32 * CELL_WIDTH + 2.0 * PAD;
        let h = DEFAULT_ROWS as f32 * CELL_HEIGHT + 2.0 * PAD;
        let attrs = ember_platform::window_attributes("Ember", w, h);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let size = window.inner_size();
        let dims = dims_for(size.width.max(1), size.height.max(1));
        let renderer = Renderer::new(Arc::clone(&window), dims);

        let session = LocalPty::spawn(LocalPtyConfig::new(SessionId::new("main"), dims))
            .expect("spawn shell");

        window.request_redraw();
        self.state = Some(RunState {
            renderer,
            session,
            modifiers: ModifiersState::empty(),
            dims,
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                let _ = state.session.control.send(BackendControl::Shutdown);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width, size.height);
                let dims = dims_for(size.width.max(1), size.height.max(1));
                if dims != state.dims {
                    state.dims = dims;
                    let _ = state.session.control.send(BackendControl::Resize(dims));
                }
            }
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state == ElementState::Pressed {
                    if let Some(bytes) = encode_key(&key.logical_key, state.modifiers) {
                        let _ = state
                            .session
                            .control
                            .send(BackendControl::Input(bytes.into_boxed_slice()));
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                while let Some(delta) = state.session.frames.take() {
                    state.renderer.apply_delta(delta);
                }
                state.renderer.render();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // Drain the semantic lane (title, exit).
        while let Ok(event) = state.session.events.try_recv() {
            match event {
                BackendEvent::Title(title) => state.renderer.window().set_title(&title),
                BackendEvent::Exited(_) => {
                    event_loop.exit();
                    return;
                }
                _ => {}
            }
        }
        // Poll the pixel lane; redraw only when something changed.
        let mut dirty = false;
        while let Some(delta) = state.session.frames.take() {
            state.renderer.apply_delta(delta);
            dirty = true;
        }
        if dirty {
            state.renderer.window().request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }
}

/// Map a key press to the bytes to send to the PTY. Covers the essentials for a
/// usable shell (printable text, Enter/Backspace/Tab/Esc, arrows, Ctrl-letter);
/// fuller keymap + IME routing land with Epic E.
fn encode_key(key: &Key, mods: ModifiersState) -> Option<Vec<u8>> {
    match key {
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => Some(vec![0x1b]),
        Key::Named(NamedKey::Space) => Some(b" ".to_vec()),
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
        Key::Character(s) => {
            if mods.control_key() {
                let c = s.chars().next()?;
                if c.is_ascii_alphabetic() {
                    return Some(vec![(c.to_ascii_lowercase() as u8) & 0x1f]);
                }
            }
            Some(s.as_bytes().to_vec())
        }
        _ => None,
    }
}
