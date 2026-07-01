//! `ember-platform` — winit + `PlatformBackend` / OS seam (design §7; ).
//!
//! The window is created here; the event loop itself lives in `ember-app`
//! (design §2: the binary owns the event loop). `PlatformBackend` is the OS
//! effect seam — clipboard, open-path/URL, hotkey — with `LinuxBackend` and
//! `MacBackend` as co-equal v1 targets (the deep AppKit polish is deferred).

pub mod menu;
pub use menu::{AppMenu, MenuAction, build_menu, menu_action};

use winit::dpi::LogicalSize;
use winit::window::{Window, WindowAttributes};

/// Pure-domain code requests OS effects; the platform impl performs them
/// (design §7). v1 impls are intentionally thin; real clipboard/open/hotkey
/// wiring lands in Epic E (and `MacBackend`, ).
pub trait PlatformBackend {
    /// Read the system clipboard (OSC 52 read policy lives in `ember-core`).
    fn clipboard_get(&self) -> Option<String>;
    /// Write the system clipboard.
    fn clipboard_set(&self, text: &str);
    /// Open a path or URL (smart-selection / semantic-history / trigger effect).
    fn open_path(&self, target: &str);
}

/// The v1 Linux platform backend (effects pending Epic E).
#[derive(Default)]
pub struct LinuxBackend;

/// The v1 macOS platform backend — co-equal run/dev target (effects pending
/// `MacBackend`, ).
#[derive(Default)]
pub struct MacBackend;

macro_rules! todo_backend {
    ($ty:ty) => {
        impl PlatformBackend for $ty {
            fn clipboard_get(&self) -> Option<String> {
                None
            }
            fn clipboard_set(&self, _text: &str) {}
            fn open_path(&self, _target: &str) {}
        }
    };
}
todo_backend!(LinuxBackend);
todo_backend!(MacBackend);

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

    #[test]
    fn backends_are_constructible() {
        // The seam stays honest on both OSes from day one (Kaylee's rule).
        assert!(LinuxBackend.clipboard_get().is_none());
        assert!(MacBackend.clipboard_get().is_none());
    }
}
