//! Hand-rolled wgpu solid-color quad pass (design §6 hybrid). Draws per-cell
//! background fills and the block cursor under the glyphon text pass. Instanced:
//! one unit quad, N per-cell `(rect, color)` instances.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    /// `x, y, w, h` in physical pixels.
    rect: [f32; 4],
    /// Linear RGBA.
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    resolution: [f32; 2],
    _pad: [f32; 2],
}

const SHADER: &str = r#"
struct Uniforms { resolution: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsIn {
    @location(0) unit: vec2<f32>,
    @location(1) rect: vec4<f32>,
    @location(2) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    let px = in.rect.xy + in.unit * in.rect.zw;
    let ndc = vec2<f32>(px.x / u.resolution.x * 2.0 - 1.0, 1.0 - px.y / u.resolution.y * 2.0);
    var out: VsOut;
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

const UNIT_QUAD: [[f32; 2]; 6] = [
    [0.0, 0.0],
    [1.0, 0.0],
    [0.0, 1.0],
    [0.0, 1.0],
    [1.0, 0.0],
    [1.0, 1.0],
];

pub struct QuadRenderer {
    pipeline: wgpu::RenderPipeline,
    unit: wgpu::Buffer,
    instances: wgpu::Buffer,
    cap: usize,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    count: u32,
}

impl QuadRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ember-quads"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ember-quads-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ember-quads-pl"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ember-quads-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: 8,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                    },
                    wgpu::VertexBufferLayout {
                        array_stride: 32,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![1 => Float32x4, 2 => Float32x4],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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
            label: Some("ember-quads-unit"),
            contents: bytemuck::cast_slice(&UNIT_QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let cap = 256;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ember-quads-instances"),
            size: (cap * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniforms = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ember-quads-uniforms"),
            contents: bytemuck::cast_slice(&[Uniforms {
                resolution: [1.0, 1.0],
                _pad: [0.0, 0.0],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ember-quads-bg"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            unit,
            instances,
            cap,
            uniforms,
            bind_group,
            count: 0,
        }
    }

    /// Upload this frame's quads. `rects` are `(rect_px, linear_rgba)`.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        resolution: (f32, f32),
        rects: &[([f32; 4], [f32; 4])],
    ) {
        queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::cast_slice(&[Uniforms {
                resolution: [resolution.0, resolution.1],
                _pad: [0.0, 0.0],
            }]),
        );

        let instances: Vec<Instance> = rects
            .iter()
            .map(|(rect, color)| Instance {
                rect: *rect,
                color: *color,
            })
            .collect();

        if instances.len() > self.cap {
            self.cap = instances.len().next_power_of_two();
            self.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ember-quads-instances"),
                size: (self.cap * std::mem::size_of::<Instance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !instances.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        }
        self.count = instances.len() as u32;
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.unit.slice(..));
        pass.set_vertex_buffer(1, self.instances.slice(..));
        pass.draw(0..6, 0..self.count);
    }
}

/// Convert one 8-bit sRGB channel to linear (for an sRGB surface target).
pub fn srgb_to_linear(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}
