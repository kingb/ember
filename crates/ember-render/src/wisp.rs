//! THE WISP — the glowing drag token shown while a tab/pane is being carried
//! across windows (release 2, surface-drag task 5). A tiny, borderless,
//! always-on-top, click-through window that follows the pointer, own its own
//! wgpu instance/device/surface entirely separate from any [`crate::renderer::
//! Renderer`] — the release 1 multi-instance spike proved this is viable.
//! Sparks-only: no glyphon, no text atlas, no grid — just the additive
//! [`crate::background::SparkRenderer`] pipeline this module already reuses
//! for the campfire backdrop, driven by its own tiny particle-cluster
//! generator ([`wisp_quads`], pure and unit-tested below).
//!
//! `ember-app` owns the wisp's *lifecycle* (creation timing, position,
//! show/hide, fade ramps) — this module is purely the rendering half: build
//! the GPU state once, then `render` one frame per call, best-effort.

use std::sync::Arc;

use ember_core::{Rgb, WispStyle};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, PresentMode,
    RequestAdapterOptions, SurfaceConfiguration, TextureFormat, TextureUsages,
};
use winit::window::Window;

use crate::background::SparkRenderer;
use crate::paint::{lerp_rgb, lin_rgba};
use crate::renderer::{ACCENT, AMBER};

// --- Per-style palettes (v0.4.1, the wisp's 5 styles) -----------------------
//
// `Ember` keeps reusing `AMBER`/`ACCENT` (the renderer's shared campfire
// palette, unchanged). The other 4 styles get their own small, self-
// contained palettes here — they're wisp-only looks, not reused elsewhere,
// so there's no reason to promote them to `renderer.rs`.

// (Coal's BODY colors live in `coal.rs`'s procedural shader now;
// only its spark palette remains here.)
/// Coal: the hottest, freshest sparks.
const COAL_HOT: Rgb = Rgb::new(0xff, 0xf1, 0xd8);
/// Coal: the vivid mid-flight spark orange (hot -> this -> deep body red),
/// and the lit faces of the glowing rock.
const COAL_SPARK: Rgb = Rgb::new(0xff, 0x8a, 0x1e);

/// Will-o'-the-wisp: pale ghost-cyan.
const WISP_CYAN: Rgb = Rgb::new(150, 220, 230);
/// Will-o'-the-wisp: pale ghost-green.
const WISP_GREEN: Rgb = Rgb::new(120, 255, 190);

/// Comet: near-white hot head.
const COMET_HEAD: Rgb = Rgb::new(255, 250, 240);
/// Comet: blue-white tail tip.
const COMET_TAIL: Rgb = Rgb::new(170, 200, 255);

/// Goo: deep molten-orange body.
const GOO_BODY: Rgb = Rgb::new(0xcf, 0x4a, 0x10);
/// Goo: bright hot center.
const GOO_HOT: Rgb = Rgb::new(255, 205, 90);

/// Returned by [`WispRenderer::new`] when this GPU/surface can't do the alpha
/// compositing the wisp needs (no `PreMultiplied`/`PostMultiplied` mode among
/// `caps.alpha_modes`), or when adapter/device/surface creation itself fails.
/// The degradation ladder's exit: the caller (`ember-app`) falls back to no
/// wisp for the rest of the session — every drag mechanic (drop resolution,
/// previews, hover) is completely unaffected either way.
#[derive(Debug)]
pub struct WispUnsupported;

impl std::fmt::Display for WispUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wisp: no supported alpha-compositing mode for this surface"
        )
    }
}

impl std::error::Error for WispUnsupported {}

/// The wisp's own tiny renderer: instance/adapter/device/surface, all
/// independent of the main window's `Renderer` (own everything, per the
/// brief — never shares a device with any pane window).
pub struct WispRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: SurfaceConfiguration,
    sparks: SparkRenderer,
    /// The `coal` style's solid procedural burning-rock body, drawn
    /// under the additive sparks. Built eagerly — one tiny pipeline, and the
    /// wisp window itself is already lazily created per drag.
    coal: crate::coal::CoalRenderer,
    // Keep the window LAST so it drops after the surface (winit/wgpu
    // requirement — mirrors `renderer::Renderer`).
    window: Arc<Window>,
}

impl WispRenderer {
    /// Build the wisp's GPU state for an existing (already-created) window.
    /// Blocks on async GPU init, same as `Renderer::new`.
    pub fn new(window: Arc<Window>) -> Result<Self, WispUnsupported> {
        pollster::block_on(Self::new_async(window))
    }

    async fn new_async(window: Arc<Window>) -> Result<Self, WispUnsupported> {
        let size = window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));

        let instance = Instance::new(InstanceDescriptor::new_with_display_handle(Box::new(
            Arc::clone(&window),
        )));
        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|_| WispUnsupported)?;
        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|_| WispUnsupported)?;
        let (device, queue) = adapter
            .request_device(&DeviceDescriptor::default())
            .await
            .map_err(|_| WispUnsupported)?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(TextureFormat::is_srgb)
            .or_else(|| caps.formats.first().copied())
            .ok_or(WispUnsupported)?;
        // The whole point of the wisp: it must composite over whatever's
        // behind it (the desktop, other windows) rather than punch an opaque
        // hole. `PreMultiplied` first (the common case — Metal/most Vulkan
        // ICDs), `PostMultiplied` as the documented fallback, else this
        // surface simply can't show a translucent window — degrade instead
        // of drawing an opaque gray box where the wisp should be.
        let alpha_mode = if caps
            .alpha_modes
            .contains(&CompositeAlphaMode::PreMultiplied)
        {
            CompositeAlphaMode::PreMultiplied
        } else if caps
            .alpha_modes
            .contains(&CompositeAlphaMode::PostMultiplied)
        {
            CompositeAlphaMode::PostMultiplied
        } else {
            return Err(WispUnsupported);
        };
        let present_mode = if caps.present_modes.contains(&PresentMode::Mailbox) {
            PresentMode::Mailbox
        } else {
            PresentMode::Fifo
        };
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let sparks = SparkRenderer::new(&device, format);
        let coal = crate::coal::CoalRenderer::new(&device, format);

        Ok(Self {
            device,
            queue,
            surface,
            config,
            sparks,
            coal,
            window,
        })
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    /// Draw one frame of the particle cluster: `style` picks which of the 5
    /// generators draws (see [`wisp_quads`]), `t` is seconds (a free-running
    /// clock — the caller never resets it), `intensity` (`0..1`, already
    /// fade-ramped by the caller) is the overall brightness/opacity, and
    /// `velocity` is the drag's current screen-space px/s (biases the trail
    /// opposite the direction of travel). Best-effort: a starved/lost
    /// surface is silently skipped or reconfigured, same policy as the main
    /// `Renderer` but with nothing to report back — the wisp is decorative,
    /// never load-bearing for drag mechanics.
    pub fn render(&mut self, style: WispStyle, t: f32, intensity: f32, velocity: (f32, f32)) {
        // The wisp window is deliberately excluded from `ember-app`'s
        // `windows` map (it's not a pane surface), so its Resized/
        // ScaleFactorChanged events never reach us — the only way this
        // renderer learns its window changed size is by asking directly,
        // here, every frame. Without this check, dragging across a
        // mixed-DPI monitor boundary leaves `self.config` holding the old
        // (stale) physical size; the Outdated/Lost branch below would then
        // reconfigure the surface with those stale dims instead of the
        // window's actual current size, and on some backends that wedges
        // the surface into a permanently blank state for the rest of the
        // drag. Compare against the window's live size and reconfigure
        // proactively, before acquiring, whenever it disagrees.
        let live = self.window.inner_size();
        if live.width > 0
            && live.height > 0
            && (live.width != self.config.width || live.height != self.config.height)
        {
            self.config.width = live.width;
            self.config.height = live.height;
            self.surface.configure(&self.device, &self.config);
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
                let _ = self.device.poll(wgpu::PollType::Poll);
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.surface.configure(&self.device, &self.config);
                let _ = self.device.poll(wgpu::PollType::Poll);
                return;
            }
        };

        let quads = wisp_quads(
            style,
            t,
            intensity,
            velocity,
            self.config.width as f32,
            self.config.height as f32,
        );
        self.sparks.prepare(
            &self.device,
            &self.queue,
            (self.config.width as f32, self.config.height as f32),
            &quads,
        );
        let is_coal = style == WispStyle::Coal;
        if is_coal {
            self.coal.prepare(
                &self.queue,
                (self.config.width as f32, self.config.height as f32),
                t,
                intensity,
            );
        }

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ember-wisp"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ember-wisp-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Fully transparent clear — only the sparks/core paint
                        // anything; everything else stays see-through.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 0.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if is_coal {
                self.coal.draw(&mut pass);
            }
            self.sparks.draw(&mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        // Same reclaim-per-present discipline as `Renderer::render` — without
        // it, per-frame staging buffers pile up (the  leak).
        let _ = self.device.poll(wgpu::PollType::Poll);
    }
}

/// Clamp `intensity` to `0..1` and early-return an empty quad list when it's
/// zero-or-below — the shared guard every style generator opens with (the
/// dispatch contract: `intensity<=0 → empty`, same signature, same `s =
/// w.min(h)` scaling).
macro_rules! guard_intensity {
    ($intensity:expr) => {{
        let i = $intensity.clamp(0.0, 1.0);
        if i <= 0.0 {
            return Vec::new();
        }
        i
    }};
}

/// Dispatch to one of the wisp's 5 visual styles (v0.4.1 — settings row
/// "Wisp style", config key `wisp_style`). Every generator shares this exact
/// signature and the additive-quad contract documented on [`ember_quads`]:
/// `(rect_px, linear_rgba)` quads, `s = w.min(h)`-relative geometry,
/// `intensity<=0 → empty`. Pure — no stored state, cheap to unit-test.
pub(crate) fn wisp_quads(
    style: WispStyle,
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    match style {
        WispStyle::Ember => ember_quads(t, intensity, velocity, w, h),
        WispStyle::Coal => coal_quads(t, intensity, velocity, w, h),
        WispStyle::WillOWisp => willowisp_quads(t, intensity, velocity, w, h),
        WispStyle::Comet => comet_quads(t, intensity, velocity, w, h),
        WispStyle::Goo => goo_quads(t, intensity, velocity, w, h),
        WispStyle::Star => star_quads(t, intensity, velocity, w, h),
    }
}

/// Pure: the wisp's particle cluster for one frame — a pulsing glowing core
/// at the window's center, a ring of sparks orbiting/attracted toward it, and
/// (once the drag has some speed) a short trail of fading quads stretched
/// back opposite `velocity`. `w`/`h` are the wisp surface's PHYSICAL px (the
/// cluster is always centered in them — the app keeps the window centered on
/// the pointer); `intensity` (already clamped/ramped by the caller) scales
/// every alpha. No stored state — same "procedural from `t` alone" shape as
/// [`crate::paint::spark_quads`], so it's cheap to unit-test. THE ORIGINAL
/// (pre-v0.4.1) wisp look, unchanged — now one of 5 styles behind
/// [`wisp_quads`]'s dispatch rather than the only one.
fn ember_quads(
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let intensity = guard_intensity!(intensity);
    let cx = w * 0.5;
    let cy = h * 0.5;
    // All cluster geometry is expressed as a fraction of the surface's min
    // dimension, so the whole wisp scales with `WISP_SIZE` (the app-side
    // window size) and is resolution-independent. Retune the fractions to
    // change the *look*; retune `WISP_SIZE` to change the *scale*.
    let s = w.min(h);
    let mut out = Vec::with_capacity(16);

    // Glowing core: one bright, slowly pulsing quad at the center.
    let pulse = 0.85 + 0.15 * (t * 6.0).sin();
    let core_size = s * 0.20 * pulse;
    let core_color = lerp_rgb(AMBER, ACCENT, 0.35 + 0.15 * (t * 3.0).sin());
    out.push((
        [
            cx - core_size * 0.5,
            cy - core_size * 0.5,
            core_size,
            core_size,
        ],
        lin_rgba(core_color, 0.95 * intensity),
    ));

    // A ring of sparks, center-attracted (small orbit radius) with a slow
    // independent drift so they don't read as a static halo.
    const N: usize = 10;
    for i in 0..N {
        let fi = i as f32;
        let hash = |a: f32, b: f32| {
            let s = ((fi * a + b).sin() * 43758.547).abs();
            s - s.floor()
        };
        let seed = hash(12.9898, 4.1);
        let angle = (fi / N as f32) * std::f32::consts::TAU + t * (0.6 + seed * 0.5);
        let orbit_r = s * (0.11 + seed * 0.23);
        let x = cx + angle.cos() * orbit_r;
        let y = cy + angle.sin() * orbit_r;
        let phase = ((t * (0.5 + seed * 0.4)) + fi / N as f32).fract();
        let color = if phase < 0.5 {
            lerp_rgb(AMBER, ACCENT, phase * 2.0)
        } else {
            lerp_rgb(ACCENT, AMBER, (phase - 0.5) * 2.0)
        };
        let flicker = 0.75 + 0.25 * (t * (7.0 + seed * 5.0) + fi).sin();
        let size = s * (0.024 + seed * 0.026);
        out.push((
            [x - size * 0.5, y - size * 0.5, size, size],
            lin_rgba(color, 0.8 * intensity * flicker.max(0.0)),
        ));
    }

    // Short trail biased opposite the direction of travel: only appears once
    // the drag has real speed, so a paused/slow drag shows just the core +
    // ring (no trail smear sitting on a stationary wisp).
    let speed = (velocity.0 * velocity.0 + velocity.1 * velocity.1).sqrt();
    if speed > 40.0 {
        let inv = 1.0 / speed;
        let (dx, dy) = (-velocity.0 * inv, -velocity.1 * inv);
        const TRAIL: usize = 4;
        for i in 1..=TRAIL {
            let f = i as f32 / TRAIL as f32;
            let dist = f * (s * 0.06 + speed.min(600.0) * 0.05);
            let x = cx + dx * dist;
            let y = cy + dy * dist;
            let size = (s * 0.05 - f * s * 0.029).max(s * 0.012);
            let a = (1.0 - f) * 0.5 * intensity;
            out.push((
                [x - size * 0.5, y - size * 0.5, size, size],
                lin_rgba(ACCENT, a),
            ));
        }
    }

    out
}

/// Coal: a solid, procedurally-rendered burning coal that throws a gentle
/// shower of embers. The rock BODY is NOT quads — it's drawn by
/// [`crate::coal::CoalRenderer`], an alpha-blended lava-rock sprite with
/// animated hot cracks, because the additive spark shader can only make soft
/// round glows (a solid lump is impossible there — it reads as ooze / gas /
/// sparkler). This function returns only the additive extras layered ON TOP of
/// that body: a soft centered ember glow, plus a modest shower of round sparks
/// rising off the top, cooling + twinkling like the ambient campfire sparks
/// ([`crate::paint::spark_quads`]). Velocity is ignored — sparks always rise.
fn coal_quads(
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let _ = velocity; // coal's sparks rise on their own; no drag trail
    let intensity = guard_intensity!(intensity);
    let cx = w * 0.5;
    let cy = h * 0.5;
    let s = w.min(h);
    let mut out = Vec::with_capacity(24);

    // Index 0: a small centered warm glow sitting under the procedural coal
    // body — keeps the shared centered-anchor invariant and adds a soft ember
    // core glow. (The solid rock itself is drawn by `CoalRenderer`, not here.)
    let pulse = 0.85 + 0.15 * (t * 4.0).sin();
    let core = s * 0.06 * pulse;
    out.push((
        [cx - core * 0.5, cy - core * 0.5, core, core],
        lin_rgba(COAL_SPARK, 0.35 * intensity),
    ));

    // The spark shower: a modest number of round embers rising off the top of
    // the rock, cooling + twinkling like the ambient campfire sparks. Kept
    // gentle (not a dense sparkler spew) now that the body carries the read.
    const N: usize = 20;
    for i in 0..N {
        let fi = i as f32;
        let hash = |a: f32, b: f32| {
            let v = ((fi * a + b).sin() * 43758.547).abs();
            v - v.floor()
        };
        let seed_a = hash(17.13, 3.7);
        let seed_b = hash(41.9, 9.2);
        let seed_c = hash(7.31, 1.3);
        let seed_d = hash(29.7, 5.9);
        let speed_mul = 0.6 + seed_b * 0.7;
        let life = ((t * (0.3 + speed_mul * 0.4)) + fi / N as f32).fract();
        let rise = life * s * 0.55;
        let cone = (seed_a - 0.5) * s * 0.16 * life;
        let turb = (t * (3.0 + seed_c * 4.0) + fi).sin() * s * 0.015 * life;
        // Born anywhere on the rock's surface (the sprite body is ~s*0.055 in
        // radius), not from a single vent point.
        let bx = (seed_a - 0.5) * s * 0.10;
        let by = (seed_d - 0.5) * s * 0.08;
        let x = cx + bx + cone + turb;
        let y = cy + by - rise;
        let color = if life < 0.35 {
            lerp_rgb(COAL_HOT, AMBER, life / 0.35)
        } else {
            lerp_rgb(AMBER, ACCENT, (life - 0.35) / 0.65)
        };
        let twinkle = 0.7 + 0.3 * (t * (8.0 + seed_c * 6.0) + fi).sin();
        let fade = (std::f32::consts::PI * life).sin().max(0.0);
        let sz = s * (0.007 + seed_b * seed_b * 0.02);
        out.push((
            [x - sz * 0.5, y - sz * 0.5, sz, sz],
            lin_rgba(color, 0.85 * intensity * fade * twinkle.max(0.0)),
        ));
    }

    out
}

/// Will-o'-the-wisp: a soft, cool, spectral orb — pale ghost-cyan/green,
/// low saturation, everything a soft glow (no hard spark quads, unlike
/// every other style). Breathes with a slow size pulse rather than
/// flickering, and trails a short wispy vapor tail of a few fading puffs
/// that undulate side-to-side (sine-offset) as they drift — opposite the
/// direction of travel while carried, or gently upward at rest, so the tail
/// never vanishes on a paused drag.
fn willowisp_quads(
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let intensity = guard_intensity!(intensity);
    let cx = w * 0.5;
    let cy = h * 0.5;
    let s = w.min(h);
    let mut out = Vec::with_capacity(8);

    // Breathing core: index 0, always exactly centered.
    let breathe = 0.85 + 0.15 * (t * 1.6).sin();
    let core = s * 0.22 * breathe;
    let core_color = lerp_rgb(WISP_CYAN, WISP_GREEN, 0.5 + 0.5 * (t * 0.7).sin());
    out.push((
        [cx - core * 0.5, cy - core * 0.5, core, core],
        lin_rgba(core_color, 0.55 * intensity),
    ));
    // A softer, larger halo behind it (lower alpha) — fakes a blur with no
    // actual blur pass.
    let halo = core * 1.7;
    out.push((
        [cx - halo * 0.5, cy - halo * 0.5, halo, halo],
        lin_rgba(core_color, 0.18 * intensity),
    ));

    // Wispy vapor tail: a few soft puffs, undulating side to side.
    let speed = (velocity.0 * velocity.0 + velocity.1 * velocity.1).sqrt();
    let (dx, dy) = if speed > 5.0 {
        let inv = 1.0 / speed;
        (-velocity.0 * inv, -velocity.1 * inv)
    } else {
        // At rest, a will-o'-the-wisp still drifts — upward, gently.
        (0.0, -1.0)
    };
    let (px, py) = (-dy, dx); // perpendicular unit vector, for the undulation
    const PUFFS: usize = 4;
    for i in 1..=PUFFS {
        let f = i as f32 / PUFFS as f32;
        let dist = f * s * 0.34;
        let undulate = (t * 2.2 + f * 5.0).sin() * s * 0.05 * f;
        let x = cx + dx * dist + px * undulate;
        let y = cy + dy * dist + py * undulate;
        let size = (core * (1.0 - f * 0.55)).max(s * 0.02);
        let a = (1.0 - f) * 0.34 * intensity;
        out.push((
            [x - size * 0.5, y - size * 0.5, size, size],
            lin_rgba(lerp_rgb(WISP_GREEN, WISP_CYAN, f), a),
        ));
    }

    out
}

/// Comet: a bright, tail-less white-hot head with a soft glow — the original
/// comet head a touch larger, with the long streaming tail removed (v0.4.1).
/// The dazzling flare-star variant is now its own [`star_quads`] style.
/// Velocity is ignored; it's a clean bright point.
fn comet_quads(
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let _ = velocity; // tail-less head only
    let intensity = guard_intensity!(intensity);
    let cx = w * 0.5;
    let cy = h * 0.5;
    let s = w.min(h);
    let mut out = Vec::with_capacity(2);

    // Head: index 0, always exactly centered — a touch larger than the
    // original comet head (was s*0.13), tail removed.
    let flicker = 0.92 + 0.08 * (t * 10.0).sin();
    let head = s * 0.16 * flicker;
    out.push((
        [cx - head * 0.5, cy - head * 0.5, head, head],
        lin_rgba(COMET_HEAD, 0.98 * intensity),
    ));
    // A soft glow around the head for punch.
    let glow = head * 1.9;
    out.push((
        [cx - glow * 0.5, cy - glow * 0.5, glow, glow],
        lin_rgba(COMET_HEAD, 0.28 * intensity),
    ));

    out
}

/// Star: a dazzling white-hot orb with a soft blue-white bloom and a lens-flare
/// sparkle — a bright, steady beacon (v0.4.1, promoted from an over-cooked
/// comet). A dense near-white core, a punchy inner glow, a large low-alpha
/// blue-white outer halo, and two thin crossing flare arms that pulse.
/// Velocity is ignored.
fn star_quads(
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let _ = velocity; // steady beacon, no streak
    let intensity = guard_intensity!(intensity);
    let cx = w * 0.5;
    let cy = h * 0.5;
    let s = w.min(h);
    let mut out = Vec::with_capacity(5);

    // Head: index 0, always exactly centered. (v0.4.1: trimmed ~20% smaller.)
    let flicker = 0.94 + 0.06 * (t * 10.0).sin();
    let head = s * 0.16 * flicker;
    out.push((
        [cx - head * 0.5, cy - head * 0.5, head, head],
        lin_rgba(COMET_HEAD, 0.98 * intensity),
    ));
    // Inner glow — near-white, punchy.
    let inner = head * 1.7;
    out.push((
        [cx - inner * 0.5, cy - inner * 0.5, inner, inner],
        lin_rgba(COMET_HEAD, 0.30 * intensity),
    ));
    // Outer halo — large, soft, blue-white bloom.
    let halo = head * 3.0;
    out.push((
        [cx - halo * 0.5, cy - halo * 0.5, halo, halo],
        lin_rgba(COMET_TAIL, 0.12 * intensity),
    ));
    // Lens-flare arms: two thin crossing quads (horizontal + vertical),
    // blue-white, slowly pulsing — the "brilliant" sparkle.
    let flare_pulse = 0.5 + 0.5 * (t * 3.0).sin().abs();
    let arm_len = head * 3.4;
    let arm_w = head * 0.10;
    out.push((
        [cx - arm_len * 0.5, cy - arm_w * 0.5, arm_len, arm_w],
        lin_rgba(COMET_TAIL, 0.35 * intensity * flare_pulse),
    ));
    out.push((
        [cx - arm_w * 0.5, cy - arm_len * 0.5, arm_w, arm_len],
        lin_rgba(COMET_TAIL, 0.35 * intensity * flare_pulse),
    ));

    out
}

/// Goo: a wobbling molten droplet (World of Goo homage) — a rounded blob
/// core faked from two overlapping quads pulsing out of phase (the combined
/// silhouette morphs frame to frame, rather than a uniform pulse), deep
/// molten orange with a bright hot center, plus a couple of slow drips
/// falling and fading below the core. Doesn't stream a velocity trail —
/// goo is viscous, not fast.
fn goo_quads(
    t: f32,
    intensity: f32,
    _velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let intensity = guard_intensity!(intensity);
    let cx = w * 0.5;
    let cy = h * 0.5;
    let s = w.min(h);
    let mut out = Vec::with_capacity(8);

    // Primary lobe: index 0, always exactly centered.
    let wobble_a = 0.9 + 0.1 * (t * 2.3).sin();
    let body = s * 0.26 * wobble_a;
    out.push((
        [cx - body * 0.5, cy - body * 0.5, body, body],
        lin_rgba(GOO_BODY, 0.75 * intensity),
    ));
    // A second lobe, out of phase and slightly offset — overlapping
    // additively with the first so the combined silhouette wobbles rather
    // than pulsing uniformly.
    let wobble_b = 0.9 + 0.1 * (t * 2.3 + std::f32::consts::PI * 0.6).sin();
    let lobe2 = s * 0.20 * wobble_b;
    let ox = (t * 1.3).sin() * s * 0.04;
    let oy = (t * 1.7 + 1.0).cos() * s * 0.03;
    out.push((
        [cx + ox - lobe2 * 0.5, cy + oy - lobe2 * 0.5, lobe2, lobe2],
        lin_rgba(GOO_BODY, 0.55 * intensity),
    ));
    // Bright hot center.
    let hot = body * 0.42;
    out.push((
        [cx - hot * 0.5, cy - hot * 0.5, hot, hot],
        lin_rgba(GOO_HOT, 0.9 * intensity),
    ));

    // A couple of embers floating UP off the molten body (v0.4.1: was slow
    // drips falling below; flipped to rising embers), scattering sideways a
    // little and cooling as they fade, looping.
    const EMBERS: usize = 2;
    for i in 0..EMBERS {
        let fi = i as f32;
        let period = 1.6 + fi * 0.4;
        let phase = ((t + fi * 0.8) / period).fract();
        let dist = phase * s * 0.5;
        let drift = (fi * 12.9898).sin() * s * 0.06 + (t * 1.5 + fi * 2.0).sin() * s * 0.03 * phase;
        let x = cx + drift;
        let y = cy - body * 0.35 - dist; // rise instead of fall
        let size = (s * 0.05 * (1.0 - phase * 0.5)).max(s * 0.01);
        let a = (1.0 - phase) * 0.6 * intensity;
        out.push((
            [x - size * 0.5, y - size * 0.5, size, size],
            lin_rgba(lerp_rgb(GOO_HOT, GOO_BODY, phase), a),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Ember (the original look, via the dispatcher) ----------------------

    #[test]
    fn zero_intensity_yields_no_quads() {
        assert!(wisp_quads(WispStyle::Ember, 1.0, 0.0, (0.0, 0.0), 140.0, 140.0).is_empty());
    }

    #[test]
    fn core_plus_ring_present_at_full_intensity() {
        let q = wisp_quads(WispStyle::Ember, 0.0, 1.0, (0.0, 0.0), 140.0, 140.0);
        // 1 core + 10 ring sparks, no trail (zero velocity).
        assert_eq!(q.len(), 11);
    }

    #[test]
    fn cluster_stays_centered_regardless_of_time() {
        for i in 0..20 {
            let t = i as f32 * 0.37;
            let q = wisp_quads(WispStyle::Ember, t, 1.0, (0.0, 0.0), 140.0, 140.0);
            let core = q[0];
            let (rx, ry, rw, rh) = (core.0[0], core.0[1], core.0[2], core.0[3]);
            let (cx, cy) = (rx + rw * 0.5, ry + rh * 0.5);
            assert!((cx - 70.0).abs() < 0.01, "core drifted off-center: {cx}");
            assert!((cy - 70.0).abs() < 0.01, "core drifted off-center: {cy}");
        }
    }

    #[test]
    fn fast_drag_adds_a_trail() {
        let idle = wisp_quads(WispStyle::Ember, 0.0, 1.0, (0.0, 0.0), 140.0, 140.0);
        let moving = wisp_quads(WispStyle::Ember, 0.0, 1.0, (500.0, 0.0), 140.0, 140.0);
        assert!(moving.len() > idle.len());
    }

    #[test]
    fn alpha_scales_with_intensity() {
        let full = wisp_quads(WispStyle::Ember, 0.5, 1.0, (0.0, 0.0), 140.0, 140.0);
        let half = wisp_quads(WispStyle::Ember, 0.5, 0.5, (0.0, 0.0), 140.0, 140.0);
        // Same particle count/positions; every alpha channel should be ~halved.
        assert_eq!(full.len(), half.len());
        for (f, h) in full.iter().zip(half.iter()) {
            assert!(
                (f.1[3] - h.1[3] * 2.0).abs() < 1e-4,
                "alpha didn't scale linearly with intensity: {} vs {}",
                f.1[3],
                h.1[3]
            );
        }
    }

    #[test]
    fn intensity_is_clamped() {
        let over = wisp_quads(WispStyle::Ember, 0.0, 5.0, (0.0, 0.0), 140.0, 140.0);
        let one = wisp_quads(WispStyle::Ember, 0.0, 1.0, (0.0, 0.0), 140.0, 140.0);
        assert_eq!(over, one);
    }

    // --- Dispatcher: routes correctly, shared contract holds for all 5 -----

    #[test]
    fn dispatcher_routes_to_the_right_generator() {
        for style in WispStyle::ALL {
            let direct = match style {
                WispStyle::Ember => ember_quads(0.3, 1.0, (10.0, 0.0), 140.0, 140.0),
                WispStyle::Coal => coal_quads(0.3, 1.0, (10.0, 0.0), 140.0, 140.0),
                WispStyle::WillOWisp => willowisp_quads(0.3, 1.0, (10.0, 0.0), 140.0, 140.0),
                WispStyle::Comet => comet_quads(0.3, 1.0, (10.0, 0.0), 140.0, 140.0),
                WispStyle::Goo => goo_quads(0.3, 1.0, (10.0, 0.0), 140.0, 140.0),
                WispStyle::Star => star_quads(0.3, 1.0, (10.0, 0.0), 140.0, 140.0),
            };
            let dispatched = wisp_quads(style, 0.3, 1.0, (10.0, 0.0), 140.0, 140.0);
            assert_eq!(
                dispatched, direct,
                "{style:?} didn't dispatch to its own generator"
            );
        }
    }

    #[test]
    fn every_style_is_empty_at_zero_intensity() {
        for style in WispStyle::ALL {
            assert!(
                wisp_quads(style, 1.0, 0.0, (0.0, 0.0), 140.0, 140.0).is_empty(),
                "{style:?} should be empty at zero intensity"
            );
        }
    }

    #[test]
    fn every_style_is_nonempty_at_full_intensity() {
        for style in WispStyle::ALL {
            let q = wisp_quads(style, 0.4, 1.0, (180.0, -120.0), 230.0, 230.0);
            assert!(
                !q.is_empty(),
                "{style:?} produced no quads at full intensity"
            );
        }
    }

    #[test]
    fn every_style_keeps_its_first_quad_exactly_centered() {
        // Every generator's index-0 quad is its anchor shape, centered
        // regardless of `t`/velocity — the one invariant all 5 styles share
        // (verified per-style below too, but this sweeps every style at
        // once against varying time).
        for style in WispStyle::ALL {
            for i in 0..10 {
                let t = i as f32 * 0.53;
                let q = wisp_quads(style, t, 1.0, (0.0, 0.0), 140.0, 140.0);
                let (rx, ry, rw, rh) = (q[0].0[0], q[0].0[1], q[0].0[2], q[0].0[3]);
                let (cx, cy) = (rx + rw * 0.5, ry + rh * 0.5);
                assert!((cx - 70.0).abs() < 0.01, "{style:?} anchor drifted: {cx}");
                assert!((cy - 70.0).abs() < 0.01, "{style:?} anchor drifted: {cy}");
            }
        }
    }

    #[test]
    fn every_style_alpha_scales_with_intensity() {
        for style in WispStyle::ALL {
            let full = wisp_quads(style, 0.6, 1.0, (50.0, 30.0), 140.0, 140.0);
            let half = wisp_quads(style, 0.6, 0.5, (50.0, 30.0), 140.0, 140.0);
            assert_eq!(
                full.len(),
                half.len(),
                "{style:?} count changed with intensity"
            );
            for (f, h) in full.iter().zip(half.iter()) {
                assert!(
                    (f.1[3] - h.1[3] * 2.0).abs() < 1e-3,
                    "{style:?}: alpha didn't scale linearly: {} vs {}",
                    f.1[3],
                    h.1[3]
                );
            }
        }
    }

    // --- Coal -----------------------------------------------------------------

    #[test]
    fn coal_has_core_glow_and_spark_shower() {
        let q = coal_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // 1 centered core glow + 20 spark dots (the solid body is the sprite).
        assert_eq!(q.len(), 21);
    }

    #[test]
    fn coal_sparks_are_not_a_shared_orbit() {
        // Unlike cinder's single-radius ring, coal's rising sparks land at
        // varied distances from center at any instant.
        let q = coal_quads(0.7, 1.0, (0.0, 0.0), 230.0, 230.0);
        let (cx, cy) = (115.0, 115.0);
        let dists: Vec<f32> = q[q.len() - 20..]
            .iter()
            .map(|(r, _)| {
                let (x, y) = (r[0] + r[2] * 0.5, r[1] + r[3] * 0.5);
                ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
            })
            .collect();
        let min = dists.iter().cloned().fold(f32::MAX, f32::min);
        let max = dists.iter().cloned().fold(f32::MIN, f32::max);
        assert!(
            max - min > 5.0,
            "coal sparks all landed at ~the same radius: {dists:?}"
        );
    }

    // --- Will-o'-the-wisp -------------------------------------------------------

    #[test]
    fn willowisp_has_core_halo_and_puffs_no_hard_sparks() {
        let q = willowisp_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // 1 core + 1 halo + 4 tail puffs — all soft glow, nothing else.
        assert_eq!(q.len(), 6);
    }

    #[test]
    fn willowisp_tail_present_even_at_rest() {
        // Unlike ember's velocity-gated trail, the vapor tail is always
        // there (drifting upward when the drag is paused).
        let idle = willowisp_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        let moving = willowisp_quads(0.0, 1.0, (400.0, 0.0), 230.0, 230.0);
        assert_eq!(idle.len(), moving.len());
        assert_eq!(idle.len(), 6);
    }

    // --- Comet ------------------------------------------------------------------

    #[test]
    fn comet_is_a_tailless_head_and_glow() {
        let q = comet_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // Just the head + a soft glow — no tail, no flare.
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn comet_ignores_velocity_no_streaming_tail() {
        let idle = comet_quads(0.3, 1.0, (0.0, 0.0), 230.0, 230.0);
        let moving = comet_quads(0.3, 1.0, (900.0, 0.0), 230.0, 230.0);
        assert_eq!(idle, moving, "comet should be indifferent to velocity now");
    }

    #[test]
    fn comet_head_and_glow_read_white() {
        let q = comet_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        for quad in &q {
            let c = quad.1; // linear [r, g, b, a]
            assert!(
                (c[0] - c[2]).abs() < 0.15,
                "comet should read white (R~=B): {c:?}"
            );
        }
    }

    // --- Star -------------------------------------------------------------------

    #[test]
    fn star_is_a_flare_bloom() {
        let q = star_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // head + inner glow + outer halo + 2 flare arms.
        assert_eq!(q.len(), 5);
    }

    #[test]
    fn star_head_is_white_halo_is_bluer() {
        let q = star_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        let head = q[0].1; // linear [r, g, b, a]
        let halo = q[2].1; // the large outer blue-white bloom
        assert!(
            (head[0] - head[2]).abs() < 0.15,
            "star head should read white: {head:?}"
        );
        assert!(
            halo[2] / halo[0].max(1e-6) > head[2] / head[0].max(1e-6),
            "halo should be bluer than the head: head {head:?}, halo {halo:?}"
        );
    }

    // --- Goo --------------------------------------------------------------------

    #[test]
    fn goo_has_two_lobes_hot_center_and_rising_embers() {
        let q = goo_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // 2 lobes + 1 hot center + 2 rising embers.
        assert_eq!(q.len(), 5);
    }

    #[test]
    fn goo_embers_float_up_not_down() {
        // The embers (last two quads) sit ABOVE the core center — rising,
        // not dripping below it.
        let q = goo_quads(0.35, 1.0, (0.0, 0.0), 230.0, 230.0);
        let cy = 115.0;
        for e in &q[3..] {
            let ey = e.0[1] + e.0[3] * 0.5;
            assert!(ey < cy, "goo ember should rise above center: {ey}");
        }
    }

    #[test]
    fn goo_ignores_velocity_no_streaming_trail() {
        let idle = goo_quads(0.3, 1.0, (0.0, 0.0), 230.0, 230.0);
        let moving = goo_quads(0.3, 1.0, (900.0, 900.0), 230.0, 230.0);
        assert_eq!(idle, moving, "goo should be indifferent to velocity");
    }

    #[test]
    fn goo_silhouette_wobbles_the_second_lobe_moves() {
        let a = goo_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        let b = goo_quads(0.9, 1.0, (0.0, 0.0), 230.0, 230.0);
        assert_ne!(a[1].0, b[1].0, "second lobe should move/wobble over time");
    }
}
