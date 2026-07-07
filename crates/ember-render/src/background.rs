//! Additive ember-spark pass (campfire aesthetic; ). Instanced like
//! `quads.rs`, but with **additive blending** and a **radial-falloff** fragment
//! shader so each instance is a soft round glow rather than a flat square — the
//! drifting embers. Draws into the same render pass as the solid quads (just a
//! `set_pipeline` between draws); no extra target. The warm backdrop gradient +
//! legibility scrim are plain alpha quads built in `paint.rs` and drawn through
//! the existing `QuadRenderer`, so only the glowing sparks need this pipeline.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    /// `x, y, w, h` in physical pixels.
    rect: [f32; 4],
    /// Linear RGBA (alpha is the spark's brightness; the shader applies a radial
    /// falloff on top so the glow is round and soft).
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
    @location(1) uv: vec2<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    let px = in.rect.xy + in.unit * in.rect.zw;
    let ndc = vec2<f32>(px.x / u.resolution.x * 2.0 - 1.0, 1.0 - px.y / u.resolution.y * 2.0);
    var out: VsOut;
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.color = in.color;
    out.uv = in.unit;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    // Soft round glow: full at center, 0 at the quad edge.
    let d = distance(in.uv, vec2<f32>(0.5, 0.5));
    let glow = 1.0 - smoothstep(0.0, 0.5, d);
    // Un-premultiplied; the additive blend multiplies rgb by this alpha.
    return vec4<f32>(in.color.rgb, in.color.a * glow);
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

/// Additive instanced-quad renderer for glowing ember sparks.
pub(crate) struct SparkRenderer {
    pipeline: wgpu::RenderPipeline,
    unit: wgpu::Buffer,
    instances: wgpu::Buffer,
    cap: usize,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    count: u32,
}

impl SparkRenderer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ember-sparks"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ember-sparks-bgl"),
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
            label: Some("ember-sparks-pl"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        // Additive blend: out = src.rgb * src.a + dst — sparks accumulate into a glow.
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            // Alpha channel uses Max, not Add: overlapping sparks (the
            // opaque-backdrop campfire case AND the wisp's tightly-clustered
            // ring/trail) would otherwise sum past 1.0. Under
            // `CompositeAlphaMode::PreMultiplied` the compositor blends with
            // `(1 - src_a)`, which goes negative once src_a > 1 and produces
            // visible dark halos at spark overlaps. Max is a no-op for the
            // single-spark / opaque-backdrop case (nothing to saturate
            // against) and only changes behavior where quads overlap, so the
            // pipeline stays shared between the backdrop and wisp uses.
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Max,
            },
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ember-sparks-pipeline"),
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
                    blend: Some(additive),
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
            label: Some("ember-sparks-unit"),
            contents: bytemuck::cast_slice(&UNIT_QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let cap = 128;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ember-sparks-instances"),
            size: (cap * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniforms = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ember-sparks-uniforms"),
            contents: bytemuck::cast_slice(&[Uniforms {
                resolution: [1.0, 1.0],
                _pad: [0.0, 0.0],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ember-sparks-bg"),
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

    /// Upload this frame's spark instances. `rects` are `(rect_px, linear_rgba)`.
    pub(crate) fn prepare(
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
                label: Some("ember-sparks-instances"),
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

    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
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

// --- Image backdrop pass -----------------------------------------------------
// A single full-surface textured quad drawn *first* (opaque, before the gradient/
// scrim/cells), so a user-supplied fire photo sits behind everything and the scrim
// quad darkens it for legibility. Mirrors `SparkRenderer`'s structure but binds a
// texture + sampler and computes fit via UV scale/offset in a uniform.

use crate::renderer::ImageFit;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImgUniforms {
    /// Multiply the [0,1] quad UV by this before sampling (the fit zoom).
    uv_scale: [f32; 2],
    /// Add after scaling (the fit centering offset).
    uv_offset: [f32; 2],
    /// `0` = clamp + letterbox bars; `1` = tile (wrap, no bars).
    mode: f32,
    _pad: [f32; 3],
}

const IMG_SHADER: &str = r#"
// Padded to 32 bytes with three scalars (a `vec3` pad would force 48-byte
// alignment and mismatch the Rust `ImgUniforms`).
struct U { uv_scale: vec2<f32>, uv_offset: vec2<f32>, mode: f32, p0: f32, p1: f32, p2: f32 };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@location(0) unit: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(unit.x * 2.0 - 1.0, 1.0 - unit.y * 2.0, 0.0, 1.0);
    out.uv = unit;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv * u.uv_scale + u.uv_offset;
    if (u.mode < 0.5) {
        // Letterbox bars (contain): outside the image → dark fill.
        if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
    }
    return textureSample(tex, samp, uv);
}
"#;

/// Full-surface textured-quad renderer for a backdrop image.
pub(crate) struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    unit: wgpu::Buffer,
    uniforms: wgpu::Buffer,
    clamp_sampler: wgpu::Sampler,
    repeat_sampler: wgpu::Sampler,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    has_image: bool,
    dims: (u32, u32),
    /// Whether the current bind group uses the repeat (tiling) sampler.
    repeat_bound: bool,
}

impl ImageRenderer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ember-image"),
            source: wgpu::ShaderSource::Wgsl(IMG_SHADER.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ember-image-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ember-image-pl"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ember-image-pipeline"),
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
                // Opaque replace — the image is the base layer; the scrim quad
                // (alpha-blended in the QuadRenderer pass) darkens it afterwards.
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
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
            label: Some("ember-image-unit"),
            contents: bytemuck::cast_slice(&UNIT_QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let uniforms = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ember-image-uniforms"),
            contents: bytemuck::cast_slice(&[ImgUniforms {
                uv_scale: [1.0, 1.0],
                uv_offset: [0.0, 0.0],
                mode: 0.0,
                _pad: [0.0; 3],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let clamp_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ember-image-clamp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let repeat_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ember-image-repeat"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Start with a 1×1 placeholder so the bind group is always valid; `draw`
        // is a no-op until `set_image` flips `has_image`.
        let view = placeholder_view(device);
        let bind_group = make_bind_group(device, &bind_layout, &uniforms, &view, &clamp_sampler);

        Self {
            pipeline,
            bind_layout,
            unit,
            uniforms,
            clamp_sampler,
            repeat_sampler,
            view,
            bind_group,
            has_image: false,
            dims: (1, 1),
            repeat_bound: false,
        }
    }

    pub(crate) fn has_image(&self) -> bool {
        self.has_image
    }

    /// Upload `rgba` (`w`×`h`, row-major RGBA8) as the backdrop texture and rebuild
    /// the bind group. No-op-safe for any size ≥ 1×1.
    pub(crate) fn set_image(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rgba: &[u8],
        w: u32,
        h: u32,
    ) {
        let w = w.max(1);
        let h = h.max(1);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ember-image-tex"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.dims = (w, h);
        self.has_image = true;
        self.repeat_bound = false;
        self.bind_group = make_bind_group(
            device,
            &self.bind_layout,
            &self.uniforms,
            &self.view,
            &self.clamp_sampler,
        );
    }

    /// Forget any image (back to the gradient/sparks path).
    pub(crate) fn clear(&mut self) {
        self.has_image = false;
    }

    /// Compute fit UVs for this frame and upload them; rebuild the bind group only
    /// when the sampler (tile vs clamp) must change. `resolution` is physical px.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        resolution: (f32, f32),
        fit: ImageFit,
    ) {
        if !self.has_image {
            return;
        }
        let (qw, qh) = resolution;
        let (iw, ih) = (self.dims.0 as f32, self.dims.1 as f32);
        let quad_aspect = (qw / qh).max(1e-4);
        let img_aspect = (iw / ih).max(1e-4);
        let s = img_aspect / quad_aspect;

        let (uv_scale, uv_offset, mode) = match fit {
            ImageFit::Stretch => ([1.0, 1.0], [0.0, 0.0], 0.0),
            ImageFit::Cover => {
                if s >= 1.0 {
                    // Image relatively wider → crop width.
                    ([1.0 / s, 1.0], [(1.0 - 1.0 / s) * 0.5, 0.0], 0.0)
                } else {
                    ([1.0, s], [0.0, (1.0 - s) * 0.5], 0.0)
                }
            }
            ImageFit::Contain => {
                if s >= 1.0 {
                    // Image relatively wider → fit width, letterbox top/bottom.
                    ([1.0, s], [0.0, (1.0 - s) * 0.5], 0.0)
                } else {
                    ([1.0 / s, 1.0], [(1.0 - 1.0 / s) * 0.5, 0.0], 0.0)
                }
            }
            ImageFit::Tile => ([qw / iw, qh / ih], [0.0, 0.0], 1.0),
        };

        queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::cast_slice(&[ImgUniforms {
                uv_scale,
                uv_offset,
                mode,
                _pad: [0.0; 3],
            }]),
        );

        let want_repeat = matches!(fit, ImageFit::Tile);
        if want_repeat != self.repeat_bound {
            let sampler = if want_repeat {
                &self.repeat_sampler
            } else {
                &self.clamp_sampler
            };
            self.bind_group = make_bind_group(
                device,
                &self.bind_layout,
                &self.uniforms,
                &self.view,
                sampler,
            );
            self.repeat_bound = want_repeat;
        }
    }

    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if !self.has_image {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.unit.slice(..));
        pass.draw(0..6, 0..1);
    }
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("ember-image-bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Create a 1×1 placeholder texture view (used before any image is set).
fn placeholder_view(device: &wgpu::Device) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ember-image-placeholder"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}
