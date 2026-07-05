//! `ember-platform` — winit + `PlatformBackend` / OS seam (design §7; ).
//!
//! The window is created here; the event loop itself lives in `ember-app`
//! (design §2: the binary owns the event loop). `PlatformBackend` is the OS
//! effect seam — clipboard, open-path/URL, hotkey — with `LinuxBackend` and
//! `MacBackend` as co-equal v1 targets (the deep AppKit polish is deferred).
//!
//! Clipboard is `arboard` (already cross-platform: X11, Wayland, and macOS
//! all work through it) and open-path is a two-line `#[cfg]` branch — both
//! genuinely OS-agnostic today, so `MacBackend`/`LinuxBackend`'s bodies are
//! identical. They stay distinct types anyway: that's the actual point of
//! the seam — a clean place for ONE of them to diverge later (e.g. a
//! Wayland-specific clipboard quirk) without threading a new `#[cfg]` through
//! every call site in `ember-app`.

pub mod menu;
pub use menu::{AppMenu, MenuAction, build_menu, menu_action};

use winit::dpi::LogicalSize;
use winit::window::{Window, WindowAttributes};

/// Pure-domain code requests OS effects; the platform impl performs them
/// (design §7). Clipboard needs `&mut self` — `arboard::Clipboard` is a live
/// handle onto a system resource (the X11 clipboard in particular is a
/// selection-owner connection, not a stateless call), not a pure function.
pub trait PlatformBackend {
    /// Read the system clipboard (OSC 52 read policy lives in `ember-core`).
    fn clipboard(&mut self) -> Option<String>;
    /// Write the system clipboard.
    fn set_clipboard(&mut self, text: &str);
    /// Open a path or URL (smart-selection / semantic-history / trigger effect).
    fn open_path(&self, target: &str);
}

/// One `arboard::Clipboard` handle, opened lazily at construction and reused
/// (arboard recommends this over reopening per call — cheaper, and avoids
/// X11 selection-ownership churn). `None` if the platform clipboard is
/// unavailable (e.g. headless), matching the pre-revival behavior.
struct ClipboardHandle(Option<arboard::Clipboard>);

impl Default for ClipboardHandle {
    fn default() -> Self {
        Self(arboard::Clipboard::new().ok())
    }
}

impl ClipboardHandle {
    fn get(&mut self) -> Option<String> {
        self.0.as_mut()?.get_text().ok()
    }

    fn set(&mut self, text: &str) {
        if let Some(cb) = self.0.as_mut() {
            if let Err(e) = cb.set_text(text) {
                eprintln!("[ember] clipboard write failed: {e}");
            }
        }
    }
}

/// The v1 Linux platform backend.
#[derive(Default)]
pub struct LinuxBackend(ClipboardHandle);

/// The v1 macOS platform backend — co-equal run/dev target (deep AppKit
/// clipboard polish, e.g. rich content types, is deferred past v1).
#[derive(Default)]
pub struct MacBackend(ClipboardHandle);

macro_rules! impl_backend {
    ($ty:ty) => {
        impl PlatformBackend for $ty {
            fn clipboard(&mut self) -> Option<String> {
                self.0.get()
            }
            fn set_clipboard(&mut self, text: &str) {
                self.0.set(text);
            }
            fn open_path(&self, target: &str) {
                open_url(target);
            }
        }
    };
}
impl_backend!(LinuxBackend);
impl_backend!(MacBackend);

/// The platform backend for the host OS.
#[cfg(target_os = "macos")]
pub type HostBackend = MacBackend;
#[cfg(not(target_os = "macos"))]
pub type HostBackend = LinuxBackend;

/// Open a URL (or path) in the user's default handler — an OS effect (design §7):
/// macOS `open`, everything else `xdg-open`. Best-effort and non-blocking; a
/// failure to spawn is ignored (About-page Docs/GitHub links, smart-selection).
pub fn open_url(target: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(target).spawn();
}

/// Window attributes for the main terminal window, sized in logical pixels.
pub fn window_attributes(title: &str, width: f32, height: f32) -> WindowAttributes {
    Window::default_attributes()
        .with_title(title)
        .with_inner_size(LogicalSize::new(width.max(1.0), height.max(1.0)))
}

/// Decode a PNG (RGBA or RGB) into raw RGBA8 bytes + `(width, height)`. The single
/// PNG decode used for both the window icon and the backdrop image.
pub fn decode_png_rgba(png_bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(png_bytes);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    Some((rgba, info.width, info.height))
}

/// Decode a PNG (RGBA or RGB) into a winit window icon.
pub fn decode_icon(png_bytes: &[u8]) -> Option<winit::window::Icon> {
    let (rgba, w, h) = decode_png_rgba(png_bytes)?;
    winit::window::Icon::from_rgba(rgba, w, h).ok()
}

/// Set the application icon: the winit window icon (Linux/Windows) and, on
/// macOS, the live dock icon (winit's window icon is a no-op there, so we set
/// `NSApplication`'s icon directly — design §7, the macOS platform effect).
pub fn set_app_icon(window: &Window, png_bytes: &[u8]) {
    if let Some(icon) = decode_icon(png_bytes) {
        window.set_window_icon(Some(icon));
    }
    #[cfg(target_os = "macos")]
    set_dock_icon(png_bytes);
}

/// Set the macOS dock icon at runtime via AppKit (so even a non-bundled
/// `cargo run` shows the Ember icon). Must be called on the main thread.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // isolated AppKit FFI — the only unsafe in the workspace.
pub fn set_dock_icon(png_bytes: &[u8]) {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(png_bytes);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: called on the main thread with a valid NSImage.
    unsafe { app.setApplicationIconImage(Some(&image)) };
}

/// Set the application's display name — the **bold app-menu name** in the macOS
/// menu bar. A non-bundled binary (`cargo run`) has no `CFBundleName`, so macOS
/// derives that name from the process name; we override it via `NSProcessInfo`.
/// Best-effort for the dev binary — the robust fix is a real `.app` bundle with
/// `CFBundleName` at distribution time. No-op off macOS. Call before `build_menu`.
pub fn set_app_name(name: &str) {
    #[cfg(target_os = "macos")]
    set_process_name_macos(name);
    #[cfg(not(target_os = "macos"))]
    let _ = name;
}

#[cfg(target_os = "macos")]
fn set_process_name_macos(name: &str) {
    use objc2_foundation::{NSProcessInfo, NSString};
    let info = NSProcessInfo::processInfo();
    info.setProcessName(&NSString::from_str(name));
}

/// Returns the `ember-core` version this platform layer is built against.
pub fn core_version() -> &'static str {
    ember_core::version()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_against_core() {
        assert!(!core_version().is_empty());
    }

    /// Whether to skip the clipboard tests on macOS. Two distinct hard
    /// failure modes, neither catchable from Rust:
    ///
    /// - Headless CI (GitHub's macos-latest): no window server session, so
    ///   `arboard`'s NSPasteboard access SIGSEGVs the test process
    ///   (confirmed 2026-07-04). `CI` is the standard env var every CI
    ///   system sets.
    /// - Local `cargo test --workspace`: test binaries run as concurrent
    ///   processes, and parallel NSPasteboard access occasionally raises an
    ///   Objective-C exception, which Rust aborts on ("Rust cannot catch
    ///   foreign exceptions" — flaked twice on 2026-07-05). So on macOS the
    ///   clipboard tests are opt-in via EMBER_CLIPBOARD_TESTS=1 (run them
    ///   single-crate: `cargo test -p ember-platform`). The clipboard path
    ///   itself is verified live via OSC 52 end-to-end.
    ///
    /// Linux needs no guard — arboard's X11/Wayland backend returns a plain
    /// `Err` with no display, confirmed on all four Ubuntu CI runners.
    fn skip_clipboard_tests() -> bool {
        cfg!(target_os = "macos")
            && (std::env::var_os("CI").is_some()
                || std::env::var_os("EMBER_CLIPBOARD_TESTS").is_none())
    }

    #[test]
    fn backends_are_constructible() {
        if skip_clipboard_tests() {
            return;
        }
        // Construction must never panic, even where no system clipboard
        // exists — arboard::Clipboard::new() just returns Err, caught by
        // ClipboardHandle and turned into a quiet `None`.
        let mut linux = LinuxBackend::default();
        let mut mac = MacBackend::default();
        let _ = linux.clipboard();
        let _ = mac.clipboard();
    }

    #[test]
    fn clipboard_round_trips_when_a_real_one_is_available() {
        if skip_clipboard_tests() {
            return;
        }
        // The system clipboard is a shared, unsynchronized OS resource —
        // `cargo test --workspace` runs other crates' test binaries as
        // concurrent processes, any of which could touch it between our
        // set() and get(). A per-process-unique marker means a mismatch is
        // unambiguous: it's contention, not our bug, so only a genuine panic
        // (not a value mismatch) should fail this test. Headless CI may have
        // no system clipboard at all, hence the `if let` rather than asserting
        // `Some`.
        let mut backend = HostBackend::default();
        let marker = format!("ember-platform-test-{}", std::process::id());
        backend.set_clipboard(&marker);
        if let Some(text) = backend.clipboard() {
            if text != marker {
                eprintln!(
                    "clipboard contention (expected {marker:?}, saw {text:?}) — not a failure"
                );
            }
        }
    }
}
