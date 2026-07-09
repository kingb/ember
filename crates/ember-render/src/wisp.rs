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

/// Coal: deep red-orange body.
const COAL_BODY: Rgb = Rgb::new(0x8a, 0x1f, 0x05);
/// Coal: a slightly brighter body tone for the outer facets (still deep,
/// not amber) so the overlapping quads read as angular, not a flat blob.
const COAL_BODY_LIT: Rgb = Rgb::new(0xb8, 0x3a, 0x0e);
/// Coal: white-hot facet cracks + emitted sparks.
const COAL_HOT: Rgb = Rgb::new(0xff, 0xf1, 0xd8);

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

        Ok(Self {
            device,
            queue,
            surface,
            config,
            sparks,
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

/// Coal: a faceted chunk of hot ember rock. The additive spark pipeline
/// only draws axis-aligned rects (no rotation), so "faceted" is faked with
/// several overlapping rects of different aspect ratios and fixed relative
/// offsets rather than a literal rotated polygon — the overlaps read as
/// angular planes catching light, not a smooth circle. White-hot cracks sit
/// at the center, brightest, pulsing faster than the body. Sparks are flung
/// outward from the core's edges on individual trajectories (each with its
/// own hashed direction/speed and a continuously looping flight phase),
/// fading as they leave — not a shared orbit like `ember_quads`' ring.
fn coal_quads(
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let _ = velocity; // coal doesn't stream a drag trail — its sparks fly on their own
    let intensity = guard_intensity!(intensity);
    let cx = w * 0.5;
    let cy = h * 0.5;
    let s = w.min(h);
    let mut out = Vec::with_capacity(16);

    // The anchor facet: index 0, always exactly centered (every style keeps
    // this invariant, so a "stays centered" test works uniformly).
    let pulse = 0.9 + 0.1 * (t * 4.0).sin();
    let body = s * 0.24 * pulse;
    out.push((
        [cx - body * 0.5, cy - body * 0.42, body, body * 0.84],
        lin_rgba(COAL_BODY, 0.85 * intensity),
    ));
    // A handful of offset, differently-proportioned facets around the
    // anchor at FIXED relative offsets (not time-varying) — only the pulse
    // and crack brightness animate, so the rock's silhouette itself doesn't
    // jitter frame to frame.
    const FACETS: [(f32, f32, f32, f32); 4] = [
        // (dx, dy, w_frac, h_frac), all as fractions of `body`.
        (-0.30, 0.10, 0.55, 0.60),
        (0.28, 0.16, 0.50, 0.55),
        (-0.08, -0.34, 0.48, 0.42),
        (0.14, 0.30, 0.46, 0.40),
    ];
    for (dx, dy, wf, hf) in FACETS {
        let fw = body * wf;
        let fh = body * hf;
        out.push((
            [cx + dx * body - fw * 0.5, cy + dy * body - fh * 0.5, fw, fh],
            lin_rgba(COAL_BODY_LIT, 0.55 * intensity),
        ));
    }

    // White-hot cracks: two thin crossing quads through the center,
    // brightest of everything, pulsing faster than the body.
    let crack_pulse = 0.7 + 0.3 * (t * 9.0).sin().abs();
    let crack_a = 0.9 * intensity * crack_pulse;
    let crack_w = body * 0.10;
    out.push((
        [cx - crack_w * 0.5, cy - body * 0.46, crack_w, body * 0.92],
        lin_rgba(COAL_HOT, crack_a),
    ));
    let crack_h = body * 0.10;
    out.push((
        [cx - body * 0.40, cy - crack_h * 0.5, body * 0.80, crack_h],
        lin_rgba(COAL_HOT, crack_a * 0.85),
    ));

    // Sparks flung outward, each on its own trajectory: a hashed direction
    // and speed, a continuously looping "flight" phase (born at the core,
    // flies out + rises, fades near the end, then a new one begins).
    const N: usize = 9;
    for i in 0..N {
        let fi = i as f32;
        let hash = |a: f32, b: f32| {
            let v = ((fi * a + b).sin() * 43758.547).abs();
            v - v.floor()
        };
        let seed_a = hash(17.13, 3.7);
        let seed_b = hash(41.9, 9.2);
        let dir = seed_a * std::f32::consts::TAU;
        let speed_mul = 0.6 + seed_b * 0.8;
        let life = ((t * (0.5 + speed_mul)) + fi / N as f32).fract();
        let dist = life * s * 0.42;
        let (dirx, diry) = (dir.cos(), dir.sin());
        let x = cx + dirx * dist;
        let y = cy + diry * dist - s * 0.10 * life; // embers rise as they fly
        let fade = (1.0 - life).powf(1.5);
        let size = s * (0.02 + seed_b * 0.02) * (1.0 - 0.4 * life);
        let color = lerp_rgb(COAL_HOT, COAL_BODY_LIT, life);
        out.push((
            [x - size * 0.5, y - size * 0.5, size, size],
            lin_rgba(color, 0.85 * intensity * fade),
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

/// Comet: a brilliant, small, white-hot head with a long dramatic tail
/// streaming opposite the direction of travel — many quads tapering in
/// size and alpha over a long distance, head white cooling to blue-white at
/// the tip. Tail length scales with speed but never drops below a minimum,
/// so it always reads as a comet even when the drag pauses.
fn comet_quads(
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
    let mut out = Vec::with_capacity(16);

    // Head: index 0, always exactly centered, small and dense.
    let flicker = 0.92 + 0.08 * (t * 10.0).sin();
    let head = s * 0.13 * flicker;
    out.push((
        [cx - head * 0.5, cy - head * 0.5, head, head],
        lin_rgba(COMET_HEAD, 0.98 * intensity),
    ));
    // A slightly larger, dimmer glow around the head for punch.
    let head_glow = head * 1.8;
    out.push((
        [
            cx - head_glow * 0.5,
            cy - head_glow * 0.5,
            head_glow,
            head_glow,
        ],
        lin_rgba(COMET_HEAD, 0.25 * intensity),
    ));

    let speed = (velocity.0 * velocity.0 + velocity.1 * velocity.1).sqrt();
    // Direction opposite travel; at rest the tail points straight down (a
    // fixed default) so it never disappears at zero velocity.
    let (dx, dy) = if speed > 1.0 {
        let inv = 1.0 / speed;
        (-velocity.0 * inv, -velocity.1 * inv)
    } else {
        (0.0, 1.0)
    };
    // Minimum tail length even at rest; grows with speed, capped so it
    // doesn't run off a small window at very high drag velocities.
    let tail_len = s * (0.55 + (speed.min(900.0) / 900.0) * 0.55);
    const TAIL: usize = 12;
    for i in 1..=TAIL {
        let f = i as f32 / TAIL as f32;
        // A little jitter along the tail so it doesn't read as a perfectly
        // uniform ladder of quads.
        let jitter = (t * 5.0 + f * 13.0).sin() * s * 0.01;
        let dist = f * tail_len;
        let x = cx + dx * dist + (-dy) * jitter;
        let y = cy + dy * dist + dx * jitter;
        let size = (head * (1.0 - f * 0.85)).max(s * 0.008);
        let a = (1.0 - f).powf(1.6) * 0.85 * intensity;
        out.push((
            [x - size * 0.5, y - size * 0.5, size, size],
            lin_rgba(lerp_rgb(COMET_HEAD, COMET_TAIL, f), a),
        ));
    }

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

    // 1-2 slow drips: fall from the underside of the core and fade out,
    // looping.
    const DRIPS: usize = 2;
    for i in 0..DRIPS {
        let fi = i as f32;
        let period = 1.6 + fi * 0.4;
        let phase = ((t + fi * 0.8) / period).fract();
        let dist = phase * s * 0.5;
        let dx = (fi * 12.9898).sin() * s * 0.06;
        let x = cx + dx;
        let y = cy + body * 0.35 + dist;
        let size = (s * 0.05 * (1.0 - phase * 0.5)).max(s * 0.01);
        let a = (1.0 - phase) * 0.6 * intensity;
        out.push((
            [x - size * 0.5, y - size * 0.5, size, size],
            lin_rgba(GOO_BODY, a),
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
    fn coal_has_body_facets_cracks_and_sparks() {
        let q = coal_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // 1 anchor + 4 facets + 2 cracks + 9 sparks.
        assert_eq!(q.len(), 16);
    }

    #[test]
    fn coal_sparks_are_not_a_shared_orbit() {
        // Unlike ember's ring (all sparks at one orbit radius from a shared
        // angle formula), coal's flung sparks should land at varied
        // distances from the center at a given instant.
        let q = coal_quads(0.7, 1.0, (0.0, 0.0), 230.0, 230.0);
        let (cx, cy) = (115.0, 115.0);
        let dists: Vec<f32> = q[7..]
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
    fn comet_has_head_glow_and_a_long_tail() {
        let q = comet_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // 1 head + 1 glow + 12 tail segments.
        assert_eq!(q.len(), 14);
    }

    #[test]
    fn comet_tail_has_a_minimum_length_at_rest() {
        let idle = comet_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // The last tail quad (tip) should still be meaningfully far from
        // center even with zero velocity — "always cometary".
        let tip = idle.last().unwrap();
        let (cx, cy) = (115.0, 115.0);
        let (x, y) = (tip.0[0] + tip.0[2] * 0.5, tip.0[1] + tip.0[3] * 0.5);
        let dist = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        assert!(dist > 40.0, "comet tail too short at rest: {dist}px");
    }

    #[test]
    fn comet_tail_grows_with_speed() {
        let slow = comet_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        let fast = comet_quads(0.0, 1.0, (800.0, 0.0), 230.0, 230.0);
        let tip_dist = |q: &[([f32; 4], [f32; 4])]| {
            let tip = q.last().unwrap();
            let (cx, cy) = (115.0, 115.0);
            let (x, y) = (tip.0[0] + tip.0[2] * 0.5, tip.0[1] + tip.0[3] * 0.5);
            ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
        };
        assert!(
            tip_dist(&fast) > tip_dist(&slow),
            "tail should lengthen with speed"
        );
    }

    #[test]
    fn comet_head_is_near_white_tail_tip_is_bluer() {
        let q = comet_quads(0.0, 1.0, (500.0, 0.0), 230.0, 230.0);
        let head = q[0].1; // linear [r, g, b, a]
        let tail = q.last().unwrap().1;
        // Head reads as (near-)white: R and B are close to each other.
        assert!(
            (head[0] - head[2]).abs() < 0.15,
            "head should read white: {head:?}"
        );
        // Tail tip is bluer than the head: B/R ratio rises toward the tip.
        assert!(
            tail[2] / tail[0].max(1e-6) > head[2] / head[0].max(1e-6),
            "tail should cool toward blue-white: head {head:?}, tail {tail:?}"
        );
    }

    // --- Goo --------------------------------------------------------------------

    #[test]
    fn goo_has_two_lobes_hot_center_and_drips() {
        let q = goo_quads(0.0, 1.0, (0.0, 0.0), 230.0, 230.0);
        // 2 lobes + 1 hot center + 2 drips.
        assert_eq!(q.len(), 5);
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
