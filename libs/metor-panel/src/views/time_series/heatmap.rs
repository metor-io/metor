//! Intensity fields and the tonemap pass that colors them.
//!
//! An intensity field is a `[cols × rows]` grid of scalars living in an
//! `R32Float` texture, stretched over the plot area by a fullscreen pass that
//! normalizes each cell and looks it up in a colormap LUT. It is the shared
//! substrate under two views: the spectrogram fills the grid on the CPU from a
//! vector component's history, and a value-density trace will later accumulate
//! into the same texture on the GPU — which is why the texture carries
//! `RENDER_ATTACHMENT` usage and the uniform carries a `gain`/`scale` pair the
//! spectrogram leaves at their identity.
//!
//! Empty cells are marked with [`EMPTY_INTENSITY`] rather than NaN: the
//! shader's fast-math reassociation makes NaN tests unreliable, while a
//! sentinel far below any real value survives it.

use bytemuck::{Pod, Zeroable};
use gpui::Hsla;

use crate::gpu_context::GpuContext;
use crate::theme::Theme;

/// Value written into a grid cell no sample covered. The tonemap pass drops
/// these to fully transparent, so the plot background shows through.
pub(crate) const EMPTY_INTENSITY: f32 = -3.0e38;

/// Cut-off separating [`EMPTY_INTENSITY`] from any real value. Mirrored by
/// `EMPTY_THRESHOLD` in `heatmap.wgsl`, so what the CPU calls empty and what
/// the shader discards are the same set of cells.
pub(crate) const EMPTY_THRESHOLD: f32 = -1.0e38;

/// Entries in the colormap lookup texture.
const LUT_SIZE: u32 = 256;

/// How intensity is colored.
///
/// `TraceColor` ramps alpha over one caller-supplied color, which keeps a
/// density trace visually tied to the line it summarizes; the other two read
/// the theme's `density_stops`, which is what a spectrogram wants — it has no
/// line to match.
#[derive(
    Clone, Copy, PartialEq, Eq, Debug, Default, facet::Facet, serde::Serialize, serde::Deserialize,
)]
#[repr(u8)]
pub enum Colormap {
    TraceColor,
    #[default]
    Heat,
    Mono,
}

/// Tone curve applied between normalization and the LUT lookup.
///
/// Telemetry magnitudes are usually spread over decades, so `Log` is the
/// default: without it a single loud bin flattens everything else to black.
#[derive(
    Clone, Copy, PartialEq, Eq, Debug, Default, facet::Facet, serde::Serialize, serde::Deserialize,
)]
#[repr(u8)]
pub enum IntensityScale {
    Linear,
    Sqrt,
    #[default]
    Log,
}

impl Colormap {
    /// Colour at `t` (0–1) along this map. Shared by the GPU LUT and the
    /// spectrogram's colorbar, so the legend cannot drift from the image.
    pub(crate) fn sample(self, theme: &Theme, trace_color: Hsla, t: f32) -> Hsla {
        match self {
            Colormap::TraceColor => Hsla {
                a: t,
                ..trace_color
            },
            Colormap::Heat => theme.density_color(t),
            Colormap::Mono => Hsla {
                s: 0.0,
                ..theme.density_color(t)
            },
        }
    }
}

impl IntensityScale {
    /// Apply the curve to a raw magnitude, for callers that fold the scale in
    /// while building a grid rather than leaving it to the shader.
    ///
    /// `Log` is decibels — the conventional unit for a spectrum — floored so a
    /// zero-magnitude bin lands at a finite value rather than `-inf`.
    pub(crate) fn apply(self, v: f64) -> f64 {
        match self {
            IntensityScale::Linear => v,
            IntensityScale::Sqrt => v.max(0.0).sqrt(),
            IntensityScale::Log => 10.0 * v.max(1e-12).log10(),
        }
    }

    /// Unit suffix for a value the curve has already been applied to.
    pub(crate) fn unit(self) -> &'static str {
        match self {
            IntensityScale::Log => " dB",
            _ => "",
        }
    }

    fn mode(self) -> u32 {
        match self {
            IntensityScale::Linear => 0,
            IntensityScale::Sqrt => 1,
            IntensityScale::Log => 2,
        }
    }
}

/// Draw inputs for one intensity field, borrowed for the frame like
/// [`LineDraw`](super::LineDraw).
///
/// `grid` is row-major `rows × cols` with row 0 at the bottom of the plot.
/// `row_view` is the visible slice of rows in grid units, so panning and
/// zooming the Y axis re-slices the same upload instead of rebuilding it.
pub(crate) struct IntensityDraw<'a> {
    pub grid: &'a [f32],
    pub cols: u32,
    pub rows: u32,
    pub lo: f32,
    pub hi: f32,
    pub gain: f32,
    pub scale: IntensityScale,
    pub colormap: Colormap,
    pub trace_color: Hsla,
    pub row_view: (f32, f32),
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TonemapUniform {
    lo: f32,
    hi: f32,
    gain: f32,
    scale_mode: u32,
    row_lo: f32,
    row_hi: f32,
    cols: u32,
    rows: u32,
}

/// The `R32Float` grid texture, sized to the caller's grid rather than to the
/// plot: the fullscreen pass does the stretching, so a 300-column grid costs
/// the same upload whatever the panel's pixel width.
struct IntensityField {
    view: wgpu::TextureView,
    texture: wgpu::Texture,
    cols: u32,
    rows: u32,
    /// Bumped on every reallocation, so cached bind groups know to rebuild.
    generation: u64,
}

impl IntensityField {
    fn new(device: &wgpu::Device, cols: u32, rows: u32, generation: u64) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("intensity field"),
            size: wgpu::Extent3d {
                width: cols.max(1),
                height: rows.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            // `RENDER_ATTACHMENT` is unused by the CPU-filled path but is what
            // lets a future accumulation pass draw into the same texture.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            view,
            texture,
            cols: cols.max(1),
            rows: rows.max(1),
            generation,
        }
    }

    /// Resize to `cols × rows`, reallocating only when the size actually
    /// changed.
    fn ensure(&mut self, device: &wgpu::Device, cols: u32, rows: u32) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if self.cols == cols && self.rows == rows {
            return;
        }
        *self = Self::new(device, cols, rows, self.generation + 1);
    }

    /// Upload a row-major grid in one `write_texture`. Rows beyond `grid`'s
    /// length are left holding whatever the previous frame wrote, which cannot
    /// happen while callers size the grid to the texture.
    fn fill_from_grid(&self, queue: &wgpu::Queue, grid: &[f32]) {
        let expected = self.cols as usize * self.rows as usize;
        let grid = &grid[..grid.len().min(expected)];
        if grid.len() < expected {
            return;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(grid),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.cols * 4),
                rows_per_image: Some(self.rows),
            },
            wgpu::Extent3d {
                width: self.cols,
                height: self.rows,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// What a built LUT depends on. The theme is keyed by pointer: palettes are
/// `static`s swapped wholesale, so identity is a sufficient test.
#[derive(Clone, Copy, PartialEq, Eq)]
struct LutKey {
    colormap: Colormap,
    theme: usize,
    color: [u32; 4],
}

fn color_bits(color: Hsla) -> [u32; 4] {
    [
        color.h.to_bits(),
        color.s.to_bits(),
        color.l.to_bits(),
        color.a.to_bits(),
    ]
}

/// 256×1 `Rgba8Unorm` colormap, rebuilt when its [`LutKey`] changes.
struct ColormapLut {
    view: wgpu::TextureView,
    key: LutKey,
}

impl ColormapLut {
    fn build(device: &wgpu::Device, queue: &wgpu::Queue, key: LutKey, theme: &Theme) -> Self {
        let mut texels = Vec::with_capacity(LUT_SIZE as usize * 4);
        for i in 0..LUT_SIZE {
            let t = i as f32 / (LUT_SIZE - 1) as f32;
            let trace_color = Hsla {
                h: f32::from_bits(key.color[0]),
                s: f32::from_bits(key.color[1]),
                l: f32::from_bits(key.color[2]),
                a: f32::from_bits(key.color[3]),
            };
            let rgba = gpui::Rgba::from(key.colormap.sample(theme, trace_color, t));
            texels.extend_from_slice(&[
                (rgba.r.clamp(0.0, 1.0) * 255.0) as u8,
                (rgba.g.clamp(0.0, 1.0) * 255.0) as u8,
                (rgba.b.clamp(0.0, 1.0) * 255.0) as u8,
                (rgba.a.clamp(0.0, 1.0) * 255.0) as u8,
            ]);
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("colormap lut"),
            size: wgpu::Extent3d {
                width: LUT_SIZE,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
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
            &texels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(LUT_SIZE * 4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: LUT_SIZE,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            key,
        }
    }
}

/// Per-caller GPU state for the tonemap pass: the grid texture, its colormap,
/// the uniform, and the bind groups over them.
pub(crate) struct HeatmapState {
    field: IntensityField,
    lut: ColormapLut,
    uniform: wgpu::Buffer,
    uniform_bg: wgpu::BindGroup,
    resource_bg: wgpu::BindGroup,
    /// `(field generation, lut key)` the cached `resource_bg` was built over.
    bound: (u64, LutKey),
}

/// Pipeline and layouts for the tonemap pass, owned by the process-wide
/// [`PlotGpu`](super::gpu::PlotGpu) alongside the line pipelines.
pub(super) struct HeatmapPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_layout: wgpu::BindGroupLayout,
    resource_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl HeatmapPipeline {
    pub(super) fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("heatmap.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("heatmap.wgsl").into()),
        });
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tonemap uniform"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let resource_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tonemap resources"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        // R32Float is never filterable; the shader reads it
                        // with `textureLoad`.
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tonemap pipeline layout"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&resource_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tonemap pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: sample_count,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("colormap sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            uniform_layout,
            resource_layout,
            sampler,
        }
    }

    /// Bring `state` in sync with `draw`: resize and refill the field, rebuild
    /// the LUT if the colormap or theme changed, and write the uniform.
    pub(super) fn prepare(
        &self,
        ctx: &GpuContext,
        state: &mut Option<HeatmapState>,
        draw: &IntensityDraw<'_>,
        theme: &Theme,
    ) {
        let key = LutKey {
            colormap: draw.colormap,
            theme: std::ptr::from_ref(theme) as usize,
            color: color_bits(draw.trace_color),
        };
        let device = &ctx.device;
        let queue = &ctx.queue;

        let state = state.get_or_insert_with(|| {
            let field = IntensityField::new(device, draw.cols, draw.rows, 0);
            let lut = ColormapLut::build(device, queue, key, theme);
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tonemap uniform"),
                size: std::mem::size_of::<TonemapUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let uniform_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tonemap uniform bg"),
                layout: &self.uniform_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            let resource_bg = self.resource_bind_group(device, &field, &lut);
            HeatmapState {
                bound: (field.generation, lut.key),
                field,
                lut,
                uniform,
                uniform_bg,
                resource_bg,
            }
        });

        state.field.ensure(device, draw.cols, draw.rows);
        if state.lut.key != key {
            state.lut = ColormapLut::build(device, queue, key, theme);
        }
        if state.bound != (state.field.generation, state.lut.key) {
            state.resource_bg = self.resource_bind_group(device, &state.field, &state.lut);
            state.bound = (state.field.generation, state.lut.key);
        }
        state.field.fill_from_grid(queue, draw.grid);

        let uniform = TonemapUniform {
            lo: draw.lo,
            hi: draw.hi,
            gain: draw.gain,
            scale_mode: draw.scale.mode(),
            row_lo: draw.row_view.0,
            row_hi: draw.row_view.1,
            cols: state.field.cols,
            rows: state.field.rows,
        };
        queue.write_buffer(&state.uniform, 0, bytemuck::bytes_of(&uniform));
    }

    fn resource_bind_group(
        &self,
        device: &wgpu::Device,
        field: &IntensityField,
        lut: &ColormapLut,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tonemap resource bg"),
            layout: &self.resource_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&field.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&lut.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    /// Record the fullscreen tonemap draw. Callers issue it before their line
    /// traces so ordinary traces overlay the field.
    pub(super) fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, state: &'a HeatmapState) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &state.uniform_bg, &[]);
        pass.set_bind_group(1, &state.resource_bg, &[]);
        pass.draw(0..3, 0..1);
    }
}
