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
