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

use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, PresentMode,
    RequestAdapterOptions, SurfaceConfiguration, TextureFormat, TextureUsages,
};
use winit::window::Window;

use crate::background::SparkRenderer;
use crate::paint::{lerp_rgb, lin_rgba};
use crate::renderer::{ACCENT, AMBER};

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

    /// Draw one frame of the particle cluster: `t` is seconds (a free-running
    /// clock — the caller never resets it), `intensity` (`0..1`, already
    /// fade-ramped by the caller) is the overall brightness/opacity, and
    /// `velocity` is the drag's current screen-space px/s (biases the trail
    /// opposite the direction of travel). Best-effort: a starved/lost
    /// surface is silently skipped or reconfigured, same policy as the main
    /// `Renderer` but with nothing to report back — the wisp is decorative,
    /// never load-bearing for drag mechanics.
    pub fn render(&mut self, t: f32, intensity: f32, velocity: (f32, f32)) {
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

/// Pure: the wisp's particle cluster for one frame — a pulsing glowing core
/// at the window's center, a ring of sparks orbiting/attracted toward it, and
/// (once the drag has some speed) a short trail of fading quads stretched
/// back opposite `velocity`. `w`/`h` are the wisp surface's PHYSICAL px (the
/// cluster is always centered in them — the app keeps the window centered on
/// the pointer); `intensity` (already clamped/ramped by the caller) scales
/// every alpha. No stored state — same "procedural from `t` alone" shape as
/// [`crate::paint::spark_quads`], so it's cheap to unit-test.
pub(crate) fn wisp_quads(
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    w: f32,
    h: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    let intensity = intensity.clamp(0.0, 1.0);
    if intensity <= 0.0 {
        return Vec::new();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_intensity_yields_no_quads() {
        assert!(wisp_quads(1.0, 0.0, (0.0, 0.0), 140.0, 140.0).is_empty());
    }

    #[test]
    fn core_plus_ring_present_at_full_intensity() {
        let q = wisp_quads(0.0, 1.0, (0.0, 0.0), 140.0, 140.0);
        // 1 core + 10 ring sparks, no trail (zero velocity).
        assert_eq!(q.len(), 11);
    }

    #[test]
    fn cluster_stays_centered_regardless_of_time() {
        for i in 0..20 {
            let t = i as f32 * 0.37;
            let q = wisp_quads(t, 1.0, (0.0, 0.0), 140.0, 140.0);
            let core = q[0];
            let (rx, ry, rw, rh) = (core.0[0], core.0[1], core.0[2], core.0[3]);
            let (cx, cy) = (rx + rw * 0.5, ry + rh * 0.5);
            assert!((cx - 70.0).abs() < 0.01, "core drifted off-center: {cx}");
            assert!((cy - 70.0).abs() < 0.01, "core drifted off-center: {cy}");
        }
    }

    #[test]
    fn fast_drag_adds_a_trail() {
        let idle = wisp_quads(0.0, 1.0, (0.0, 0.0), 140.0, 140.0);
        let moving = wisp_quads(0.0, 1.0, (500.0, 0.0), 140.0, 140.0);
        assert!(moving.len() > idle.len());
    }

    #[test]
    fn alpha_scales_with_intensity() {
        let full = wisp_quads(0.5, 1.0, (0.0, 0.0), 140.0, 140.0);
        let half = wisp_quads(0.5, 0.5, (0.0, 0.0), 140.0, 140.0);
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
        let over = wisp_quads(0.0, 5.0, (0.0, 0.0), 140.0, 140.0);
        let one = wisp_quads(0.0, 1.0, (0.0, 0.0), 140.0, 140.0);
        assert_eq!(over, one);
    }
}
