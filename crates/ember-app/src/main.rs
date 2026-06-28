//! `ember-term` — the Ember terminal binary (design §2).
//!
//! Empty-but-real entry point: prints the assembled version banner so the
//! full crate graph is exercised end-to-end. The winit/wgpu event loop
//! lands in Epic B.

fn main() {
    println!(
        "ember-term {} (core {}, session {}, render {}, platform {})",
        env!("CARGO_PKG_VERSION"),
        ember_core::version(),
        ember_session::core_version(),
        ember_render::core_version(),
        ember_platform::core_version(),
    );
}
