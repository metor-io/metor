//! GPU-accelerated line trace renderer for [`TimeSeriesPlot`].
//!
//! Maintains a wgpu device + line pipeline, draws all visible line traces into
//! an offscreen `Bgra8Unorm` texture, reads the bytes back, and wraps them in
//! a fresh [`gpui::RenderImage`] each frame so the caller can blit via
//! `Window::paint_image`. The macOS zero-copy IOSurface path is intentionally
//! left as a follow-up; this module is structured so it can slot in alongside
//! the readback path without changing callers.

// `bytemuck::Pod`/`Zeroable` derives expand into helper items rustc can't
// see through, producing spurious dead-code warnings on the uniform structs
// whose fields are only read via `bytemuck::bytes_of`.
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use bytemuck::{Pod, Zeroable};
use gpui::{Bounds, Hsla, Pixels, RenderImage};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::Component;
use metor_db::time_series::TimeSeriesNodeSlice;
use metor_proto::types::{PrimType, Timestamp};
use smallvec::SmallVec;

use super::PlotBounds;

const MAX_POINTS: u32 = 1 << 18;
const VALUE_BUF_BYTES: u64 = MAX_POINTS as u64 * 4;
const INDEX_BUF_BYTES: u64 = MAX_POINTS as u64 * 4;
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// One trace's worth of data the renderer needs each frame.
pub(super) struct LineDraw<'a> {
    pub component: &'a Component,
    pub element_index: usize,
    pub color: Hsla,
    pub stroke_width: f32,
}

/// Lazily-initialized wgpu adapter/device shared across all `LineRenderer`s.
struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl GpuContext {
    fn get() -> Option<Arc<GpuContext>> {
        static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
        CTX.get_or_init(|| pollster::block_on(Self::create()).ok().map(Arc::new))
            .clone()
    }

    async fn create() -> Result<GpuContext, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| "no wgpu adapter".to_string())?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("metor-panel line renderer"),
                    required_features: wgpu::Features::empty(),
                    required_limits: adapter.limits(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .map_err(|e| format!("wgpu request_device failed: {e:?}"))?;
        Ok(GpuContext { device, queue })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ViewUniform {
    scale: [f32; 2],
    offset: [f32; 2],
    viewport: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LineUniform {
    color: [f32; 4],
    line_width: f32,
    _pad: [f32; 3],
}

struct RenderTarget {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    staging: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

impl RenderTarget {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("line target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let unpadded = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded.div_ceil(align) * align;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line staging"),
            size: padded_bytes_per_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self { width, height, texture, view, staging, padded_bytes_per_row }
    }
}

/// Owns the wgpu pipeline + per-plot scratch buffers and renders all line
/// traces for one [`TimeSeriesPlot`] into an in-memory image each frame.
pub(super) struct LineRenderer {
    ctx: Arc<GpuContext>,
    pipeline: wgpu::RenderPipeline,
    view_buf: wgpu::Buffer,
    view_bg: wgpu::BindGroup,
    line_buf: wgpu::Buffer,
    line_bg: wgpu::BindGroup,
    x_buf: wgpu::Buffer,
    y_buf: wgpu::Buffer,
    idx_buf: wgpu::Buffer,
    storage_bg: wgpu::BindGroup,
    target: Option<RenderTarget>,
    x_scratch: Vec<f32>,
    y_scratch: Vec<f32>,
    idx_scratch: Vec<u32>,
}

impl LineRenderer {
    pub(super) fn try_new() -> Option<Self> {
        let ctx = GpuContext::get()?;
        let device = &ctx.device;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("line.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("line.wgsl").into()),
        });

        let view_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("view"),
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
        let line_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("line uniform"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let storage_entry = |b| wgpu::BindGroupLayoutEntry {
            binding: b,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let storage_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("line storage"),
            entries: &[storage_entry(0), storage_entry(1), storage_entry(2)],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("line pipeline"),
            bind_group_layouts: &[&view_layout, &line_layout, &storage_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("line pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let view_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("view uniform"),
            size: std::mem::size_of::<ViewUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("view bg"),
            layout: &view_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: view_buf.as_entire_binding(),
            }],
        });

        let line_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line uniform"),
            size: std::mem::size_of::<LineUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let line_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("line uniform bg"),
            layout: &line_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: line_buf.as_entire_binding(),
            }],
        });

        let make_storage = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: VALUE_BUF_BYTES,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let x_buf = make_storage("line x");
        let y_buf = make_storage("line y");
        let idx_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line indices"),
            size: INDEX_BUF_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let storage_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("line storage bg"),
            layout: &storage_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: x_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: y_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: idx_buf.as_entire_binding() },
            ],
        });

        Some(Self {
            ctx,
            pipeline,
            view_buf,
            view_bg,
            line_buf,
            line_bg,
            x_buf,
            y_buf,
            idx_buf,
            storage_bg,
            target: None,
            x_scratch: Vec::with_capacity(MAX_POINTS as usize),
            y_scratch: Vec::with_capacity(MAX_POINTS as usize),
            idx_scratch: Vec::with_capacity(MAX_POINTS as usize),
        })
    }

    /// Draw all `traces` into the offscreen target sized to `bounds` and
    /// return the result as an image suitable for `Window::paint_image`.
    /// Returns `None` if there is nothing visible to draw.
    pub(super) fn render(
        &mut self,
        bounds: Bounds<Pixels>,
        view: PlotBounds,
        scale_factor: f32,
        traces: &[LineDraw<'_>],
    ) -> Option<Arc<RenderImage>> {
        let scale = scale_factor.max(1.0);
        let width = ((f32::from(bounds.size.width) * scale).round() as u32).max(1);
        let height = ((f32::from(bounds.size.height) * scale).round() as u32).max(1);
        if width == 0 || height == 0 || traces.is_empty() {
            return None;
        }

        if self.target.as_ref().is_none_or(|t| t.width != width || t.height != height) {
            self.target = Some(RenderTarget::new(&self.ctx.device, width, height));
        }
        let target = self.target.as_ref()?;

        let epoch = view.min_x as i64;
        let dx = (view.max_x - view.min_x).max(1e-12);
        let dy = (view.max_y - view.min_y).max(1e-12);
        let view_uniform = ViewUniform {
            scale: [(2.0 / dx) as f32, (2.0 / dy) as f32],
            offset: [-1.0, -1.0 - (view.min_y * 2.0 / dy) as f32],
            viewport: [width as f32, height as f32],
            _pad: [0.0; 2],
        };
        self.ctx.queue.write_buffer(&self.view_buf, 0, bytemuck::bytes_of(&view_uniform));

        let mut any_drawn = false;
        for trace in traces.iter() {
            let count = collect_decimated(
                trace.component,
                trace.element_index,
                view,
                width as usize,
                epoch,
                &mut self.x_scratch,
                &mut self.y_scratch,
            );
            if count < 2 {
                continue;
            }
            self.idx_scratch.clear();
            self.idx_scratch.extend(0..count as u32);

            self.ctx.queue.write_buffer(
                &self.x_buf, 0, bytemuck::cast_slice(&self.x_scratch),
            );
            self.ctx.queue.write_buffer(
                &self.y_buf, 0, bytemuck::cast_slice(&self.y_scratch),
            );
            self.ctx.queue.write_buffer(
                &self.idx_buf, 0, bytemuck::cast_slice(&self.idx_scratch),
            );

            let rgba = trace.color.to_rgb();
            let line_uniform = LineUniform {
                color: [rgba.r, rgba.g, rgba.b, rgba.a],
                line_width: trace.stroke_width,
                _pad: [0.0; 3],
            };
            self.ctx.queue.write_buffer(&self.line_buf, 0, bytemuck::bytes_of(&line_uniform));

            let load = if !any_drawn {
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
            } else {
                wgpu::LoadOp::Load
            };
            let mut encoder =
                self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("line encoder"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("line pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target.view,
                        resolve_target: None,
                        ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.view_bg, &[]);
                pass.set_bind_group(1, &self.line_bg, &[]);
                pass.set_bind_group(2, &self.storage_bg, &[]);
                pass.draw(0..4, 0..(count as u32 - 1));
            }
            self.ctx.queue.submit(Some(encoder.finish()));
            any_drawn = true;
        }

        if !any_drawn {
            return None;
        }

        let mut encoder =
            self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("line readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &target.staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.padded_bytes_per_row),
                    rows_per_image: Some(target.height),
                },
            },
            wgpu::Extent3d { width: target.width, height: target.height, depth_or_array_layers: 1 },
        );
        self.ctx.queue.submit(Some(encoder.finish()));

        let bytes = read_staging(&self.ctx.device, target)?;
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(target.width, target.height, bytes)?;
        let mut frames = SmallVec::<[Frame; 1]>::new();
        frames.push(Frame::new(buffer));
        Some(Arc::new(RenderImage::new(frames)))
    }
}

fn read_staging(device: &wgpu::Device, target: &RenderTarget) -> Option<Vec<u8>> {
    let slice = target.staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    let _ = device.poll(wgpu::Maintain::Wait);
    rx.recv().ok()?.ok()?;

    let unpadded_row = target.width as usize * 4;
    let padded_row = target.padded_bytes_per_row as usize;
    let mut out = Vec::with_capacity(unpadded_row * target.height as usize);
    {
        let view = slice.get_mapped_range();
        for row in 0..target.height as usize {
            let start = row * padded_row;
            out.extend_from_slice(&view[start..start + unpadded_row]);
        }
    }
    target.staging.unmap();
    Some(out)
}

/// Walk a component's visible time range, two-level decimate to fit
/// `pixel_budget`, and push (timestamp − epoch, value) pairs into the
/// supplied scratch vectors. Returns the number of points written.
fn collect_decimated(
    component: &Component,
    element_index: usize,
    view: PlotBounds,
    pixel_budget: usize,
    epoch: i64,
    xs: &mut Vec<f32>,
    ys: &mut Vec<f32>,
) -> usize {
    xs.clear();
    ys.clear();
    if pixel_budget == 0 {
        return 0;
    }
    let cap = MAX_POINTS as usize;
    let start = Timestamp(view.min_x as i64);
    let end = Timestamp(view.max_x as i64);
    let Some(slice) = component.time_series.get_range(start..end) else {
        return 0;
    };
    let nodes: Vec<_> = slice.as_iter().collect();
    let total: usize = nodes.iter().map(|n| n.timestamps().len()).sum();
    let elem_size = component.schema.size();
    let prim_size = component.schema.prim_type.size();

    fn fill<T: super::PlotValue>(
        nodes: &[TimeSeriesNodeSlice],
        total: usize,
        budget: usize,
        cap: usize,
        elem_size: usize,
        elem_index: usize,
        prim_size: usize,
        epoch: i64,
        xs: &mut Vec<f32>,
        ys: &mut Vec<f32>,
    ) {
        for n in nodes.iter().rev() {
            let Some(stride) = super::node_stride(n.timestamps().len(), total, budget) else {
                continue;
            };
            let timestamps = n.timestamps();
            let data = n.data();
            let mut i = 0;
            while i < timestamps.len() {
                if xs.len() >= cap {
                    return;
                }
                if let Some(v) =
                    super::read_value::<T>(data, i, elem_size, elem_index, prim_size)
                {
                    xs.push((timestamps[i].0 - epoch) as f32);
                    ys.push(v as f32);
                }
                i += stride;
            }
        }
    }

    match component.schema.prim_type {
        PrimType::F64 => fill::<f64>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::F32 => fill::<f32>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::I64 => fill::<i64>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::I32 => fill::<i32>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::I16 => fill::<i16>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::I8  => fill::<i8> (&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::U64 => fill::<u64>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::U32 => fill::<u32>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::U16 => fill::<u16>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
        PrimType::U8 | PrimType::Bool => fill::<u8>(&nodes, total, pixel_budget, cap, elem_size, element_index, prim_size, epoch, xs, ys),
    }
    xs.len()
}
