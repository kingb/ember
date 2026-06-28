//! `ember-term` — the Ember terminal binary (design §2).
//!
//! Empty-but-real entry point: prints the assembled version banner so the
//! full crate graph is exercised end-to-end. The winit/wgpu event loop
//! lands in Epic B.

fn main() {
    println!(
        "ember-term {} (core {}, session {}, render {}, platform {})",
        env!("CARGO_PKG_VERSION"),
        vt_core::version(),
        vt_session::core_version(),
        vt_render::core_version(),
        vt_platform::core_version(),
    );
}
