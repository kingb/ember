//! `ember-platform` — winit + `PlatformBackend` / OS seam (design §7; ).
//!
//! The window is created here; the event loop itself lives in `ember-app`
//! (design §2: the binary owns the event loop). `PlatformBackend` is the OS
//! effect seam — clipboard, open-path/URL, hotkey — with `LinuxBackend` and
//! `MacBackend` as co-equal v1 targets (the deep AppKit polish is deferred).

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

/// Window attributes for the main terminal window, sized in logical pixels.
pub fn window_attributes(title: &str, width: f32, height: f32) -> WindowAttributes {
    Window::default_attributes()
        .with_title(title)
        .with_inner_size(LogicalSize::new(width.max(1.0), height.max(1.0)))
}

/// Decode a PNG (RGBA or RGB) into a winit window icon.
pub fn decode_icon(png_bytes: &[u8]) -> Option<winit::window::Icon> {
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
    winit::window::Icon::from_rgba(rgba, info.width, info.height).ok()
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
