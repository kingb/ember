//! Procedural "burning coal" sprite pass for the `coal` wisp style.
//!
//! The additive spark pass ([`crate::background::SparkRenderer`]) can only draw
//! soft round glows, so a *solid* lump of coal is impossible there — it always
//! reads as glowing dots (ooze / gas / sparkler). This pass instead draws a
//! SINGLE alpha-blended (premultiplied) quad whose fragment shader renders a
//! faceted, glowing lava-rock procedurally: a lumpy silhouette, per-cell facet
//! shading, and animated white-hot cracks that pulse with `time` so the rock
//! reads as *burning*. The spark shower (still the additive `SparkRenderer`,
//! fed by [`crate::wisp::wisp_quads`]) is drawn on top.
//!
//! Premultiplied-alpha output so it composites correctly over both the opaque
//! preview backdrop and the live wisp window's transparent, premultiplied
//! surface.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    resolution: [f32; 2],
    time: f32,
    intensity: f32,
}

const UNIT_QUAD: [[f32; 2]; 6] = [
    [0.0, 0.0],
    [1.0, 0.0],
    [0.0, 1.0],
    [0.0, 1.0],
    [1.0, 0.0],
    [1.0, 1.0],
];

const SHADER: &str = r#"
struct U { resolution: vec2<f32>, time: f32, intensity: f32 };
@group(0) @binding(0) var<uniform> u: U;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@location(0) unit: vec2<f32>) -> VsOut {
    // Full-target quad; the rock is shaped in the fragment shader.
    let ndc = unit * 2.0 - vec2<f32>(1.0, 1.0);
    var out: VsOut;
    out.pos = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    out.uv = unit;
    return out;
}

fn hash21(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}
// Smooth value noise, for surface grain + a rocky (non-sinusoidal) outline.
fn noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let s = f * f * (3.0 - 2.0 * f);
    return mix(
        mix(hash21(i), hash21(i + vec2<f32>(1.0, 0.0)), s.x),
        mix(hash21(i + vec2<f32>(0.0, 1.0)), hash21(i + vec2<f32>(1.0, 1.0)), s.x),
        s.y);
}
fn hash22(p: vec2<f32>) -> vec2<f32> {
    return fract(sin(vec2<f32>(dot(p, vec2<f32>(127.1, 311.7)),
                               dot(p, vec2<f32>(269.5, 183.3)))) * 43758.5453);
}

// Voronoi: returns (F2-F1 border distance, nearest-cell hash). Small border
// distance == a crack between facets.
fn voronoi(p: vec2<f32>) -> vec2<f32> {
    let n = floor(p);
    let f = fract(p);
    var f1 = 8.0;
    var f2 = 8.0;
    var cellh = 0.0;
    for (var j = -1; j <= 1; j = j + 1) {
        for (var i = -1; i <= 1; i = i + 1) {
            let g = vec2<f32>(f32(i), f32(j));
            let o = hash22(n + g);
            let r = g + o - f;
            let d = dot(r, r);
            if (d < f1) {
                f2 = f1;
                f1 = d;
                cellh = hash21(n + g);
            } else if (d < f2) {
                f2 = d;
            }
        }
    }
    return vec2<f32>(sqrt(f2) - sqrt(f1), cellh);
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    // Center-origin coords, scaled 4.4x so the rock lands at ~22% of its
    // original footprint (half of goo-sized) while detail scales with it.
    let p = (in.uv - vec2<f32>(0.5, 0.5)) * 8.8;
    let ang = atan2(p.y, p.x);
    let r = length(p);

    // Rocky silhouette: mild low-freq lumps + directional noise so the
    // outline reads as a chipped rock, not a smooth flower.
    let dirn = p / max(r, 0.001);
    let edge_noise = noise(dirn * 2.6 + vec2<f32>(7.3, 3.1)) - 0.5;
    let lump = 0.50
        + 0.05 * sin(ang * 5.0 + 1.3)
        + 0.03 * sin(ang * 9.0 - 2.1)
        + 0.10 * edge_noise;
    let aa = 11.0 / u.resolution.y; // in scaled space (4.4x), ~2.5px on screen
    let mask = 1.0 - smoothstep(lump - aa, lump + aa, r);
    if (mask <= 0.001) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    // Facets + cracks via voronoi.
    let vor = voronoi(p * 7.0);
    let crack = vor.x;
    let cellh = vor.y;

    // Charcoal body: near-black, per-facet variation, fine noise grain so
    // faces read as rough burnt rock instead of flat cartoon tiles.
    let grain = 0.75 + 0.5 * noise(p * 16.0);
    let facet = 0.55 + 0.45 * cellh;
    var col = vec3<f32>(0.16, 0.045, 0.02) * facet * grain;

    // Heat lives IN the cracks: only most cracks glow (per-cell gate keeps
    // some dark), strongest toward the middle, each pulsing on its own
    // phase — fire inside the rock, not an evenly-lit lattice.
    let depth = smoothstep(lump, lump * 0.25, r); // 0 at rim -> 1 at center
    let gate = smoothstep(0.25, 0.55, cellh);
    let pulse = 0.6 + 0.4 * sin(u.time * 2.2 + cellh * 6.2831);
    let heat = smoothstep(0.10, 0.0, crack) * gate * (0.35 + 0.65 * depth) * pulse;
    let hot = mix(vec3<f32>(1.0, 0.30, 0.03), vec3<f32>(1.0, 0.78, 0.42), heat);
    col = col + hot * heat * 1.8;

    // Ember core breathing through the body.
    let core = smoothstep(0.55, 0.0, r) * (0.35 + 0.20 * sin(u.time * 1.4));
    col = col + vec3<f32>(1.0, 0.35, 0.06) * core * grain * 0.8;

    // Dark rounded rim.
    col = col * mix(0.35, 1.0, smoothstep(lump, lump - 0.30, r));

    // Premultiplied output.
    let a = mask * u.intensity;
    return vec4<f32>(col * a, a);
}
"#;

/// Draws the procedural burning-coal body (one quad) under the spark shower.
pub(crate) struct CoalRenderer {
    pipeline: wgpu::RenderPipeline,
    unit: wgpu::Buffer,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl CoalRenderer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ember-coal"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ember-coal-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ember-coal-pl"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        // Premultiplied-alpha over blend: out = src + dst * (1 - src.a).
        let blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ember-coal-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 8,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let unit = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ember-coal-unit"),
            contents: bytemuck::cast_slice(&UNIT_QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let uniforms = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ember-coal-uniforms"),
            contents: bytemuck::cast_slice(&[Uniforms {
                resolution: [1.0, 1.0],
                time: 0.0,
                intensity: 1.0,
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ember-coal-bg"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            unit,
            uniforms,
            bind_group,
        }
    }

    pub(crate) fn prepare(
        &self,
        queue: &wgpu::Queue,
        resolution: (f32, f32),
        time: f32,
        intensity: f32,
    ) {
        queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::cast_slice(&[Uniforms {
                resolution: [resolution.0, resolution.1],
                time,
                intensity: intensity.clamp(0.0, 1.0),
            }]),
        );
    }

    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.unit.slice(..));
        pass.draw(0..6, 0..1);
    }
}
