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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::OnceLock;

use bytemuck::{Pod, Zeroable};
use gpui::{Bounds, Hsla, Pixels, RenderImage};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::time_series::TimeSeriesNodeSlice;
use metor_db::{Component, ComponentSchema};
use metor_proto::types::{ComponentId, PrimType, Timestamp};
use offset_allocator::{Allocation, Allocator};
use smallvec::SmallVec;

use super::PlotBounds;

const VALUE_CAPACITY: u32 = 1 << 22;
const VALUE_BUF_BYTES: u64 = VALUE_CAPACITY as u64 * 4;
const INDEX_CAPACITY: u32 = 1 << 18;
const INDEX_BUF_BYTES: u64 = INDEX_CAPACITY as u64 * 4;
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
const SAMPLE_COUNT: u32 = 4;
const NS_PER_SEC: f64 = 1.0e9;

/// One trace's worth of data the renderer needs each frame.
pub(super) struct LineDraw<'a> {
    pub component_id: ComponentId,
    pub component: &'a Component,
    pub element_index: usize,
    pub color: Hsla,
    pub stroke_width: f32,
}

/// Stable identity for a cached chunk: one element column of one node.
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
struct ChunkKey {
    component_id: u64,
    element_index: u32,
    node_id: usize,
}

/// A node's worth of converted f32 samples currently resident in the value buffers.
struct ResidentChunk {
    allocation: Allocation,
    capacity: u32,
    sample_count: u32,
}

/// Per-plot offset-allocator cache mapping each visible node to a region of the
/// shared `x_buf` / `y_buf`. Once a chunk is uploaded it stays resident until it
/// either grows past its allocation or the allocator runs out of space.
struct ValueCache {
    allocator: Allocator,
    resident: HashMap<ChunkKey, ResidentChunk>,
    /// Reference epoch for the x axis, in nanoseconds. All cached x values are
    /// stored as `f32` seconds relative to this. Set lazily on first upload so
    /// the dynamic range stays small enough for `f32` to keep useful precision.
    epoch_ns: Option<i64>,
}

impl ValueCache {
    fn new() -> Self {
        Self {
            allocator: Allocator::new(VALUE_CAPACITY),
            resident: HashMap::new(),
            epoch_ns: None,
        }
    }

    fn evict_all(&mut self) {
        for (_, chunk) in self.resident.drain() {
            self.allocator.free(chunk.allocation);
        }
    }

    /// Ensure the full node behind `slice` is resident for the given trace.
    /// Returns `(offset, sample_count)` into the shared value buffers.
    fn ensure(
        &mut self,
        queue: &wgpu::Queue,
        x_buf: &wgpu::Buffer,
        y_buf: &wgpu::Buffer,
        scratch_x: &mut Vec<f32>,
        scratch_y: &mut Vec<f32>,
        component_id: ComponentId,
        element_index: usize,
        slice: &TimeSeriesNodeSlice,
        schema: &ComponentSchema,
    ) -> Option<(u32, u32)> {
        let timestamps = slice.full_timestamps();
        let len = timestamps.len();
        if len == 0 {
            return None;
        }
        if self.epoch_ns.is_none() {
            self.epoch_ns = Some(timestamps[0].0);
        }
        let epoch = self.epoch_ns?;

        let key = ChunkKey {
            component_id: component_id.0,
            element_index: element_index as u32,
            node_id: slice.node_id(),
        };

        if let Some(chunk) = self.resident.get(&key) {
            if chunk.sample_count as usize >= len {
                return Some((chunk.allocation.offset, chunk.sample_count));
            }
            if (len as u32) > chunk.capacity {
                let removed = self.resident.remove(&key).unwrap();
                self.allocator.free(removed.allocation);
            }
        }

        if !self.resident.contains_key(&key) {
            let want = (len as u32).next_power_of_two().max(64);
            let allocation = match self.allocator.allocate(want) {
                Some(a) => a,
                None => {
                    self.evict_all();
                    self.allocator.allocate(want)?
                }
            };
            self.resident.insert(
                key,
                ResidentChunk {
                    allocation,
                    capacity: want,
                    sample_count: 0,
                },
            );
        }

        let chunk = self.resident.get_mut(&key)?;
        let from = chunk.sample_count as usize;
        if from < len {
            convert_timestamps(&timestamps[from..len], epoch, scratch_x);
            convert_values(
                schema,
                slice.full_data(),
                element_index,
                from,
                len,
                scratch_y,
            );
            let byte_offset = (chunk.allocation.offset + from as u32) as u64 * 4;
            queue.write_buffer(x_buf, byte_offset, bytemuck::cast_slice(scratch_x));
            queue.write_buffer(y_buf, byte_offset, bytemuck::cast_slice(scratch_y));
            chunk.sample_count = len as u32;
        }
        Some((chunk.allocation.offset, chunk.sample_count))
    }
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
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| format!("no wgpu adapter: {e:?}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("metor-panel line renderer"),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::Performance,
                ..Default::default()
            })
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
    resolve_texture: wgpu::Texture,
    msaa_view: wgpu::TextureView,
    resolve_view: wgpu::TextureView,
    staging: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

impl RenderTarget {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let msaa_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("line msaa"),
            size: extent,
            mip_level_count: 1,
            sample_count: SAMPLE_COUNT,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let resolve_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("line resolve"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let msaa_view = msaa_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let resolve_view = resolve_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let unpadded = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded.div_ceil(align) * align;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line staging"),
            size: padded_bytes_per_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            width,
            height,
            resolve_texture,
            msaa_view,
            resolve_view,
            staging,
            padded_bytes_per_row,
        }
    }
}

/// Handle returned by [`LineRenderer::render_to_gpu`] when a new frame is
/// submitted. Callers use it on a background thread to wait for the GPU,
/// read the mapped staging buffer, and build a `RenderImage`.
pub(super) struct ReadbackHandle {
    ctx: Arc<GpuContext>,
    staging: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    in_flight: Arc<AtomicBool>,
}

impl ReadbackHandle {
    /// Block the current thread until the GPU finishes, then read the mapped
    /// staging bytes and build a `RenderImage`. Call this from a background
    /// thread (e.g. via `BackgroundExecutor::spawn`).
    pub(super) fn read_image(self) -> Option<Arc<RenderImage>> {
        let _ = self.ctx.device.poll(wgpu::PollType::wait_indefinitely());
        let bytes = read_mapped_bytes(
            &self.staging,
            self.width,
            self.height,
            self.padded_bytes_per_row,
        )?;
        self.staging.unmap();
        self.in_flight.store(false, Ordering::Release);
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(self.width, self.height, bytes)?;
        let frames = SmallVec::from_elem(Frame::new(buffer), 1);
        Some(Arc::new(RenderImage::new(frames)))
    }
}

/// Owns the wgpu pipeline, the per-plot value cache, and the offscreen target.
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
    cache: ValueCache,
    upload_x: Vec<f32>,
    upload_y: Vec<f32>,
    idx_scratch: Vec<u32>,
    readback_in_flight: Arc<AtomicBool>,
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
            bind_group_layouts: &[
                Some(&view_layout),
                Some(&line_layout),
                Some(&storage_layout),
            ],
            immediate_size: 0,
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
            multisample: wgpu::MultisampleState {
                count: SAMPLE_COUNT,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
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
            multiview_mask: None,
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
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: x_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: y_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: idx_buf.as_entire_binding(),
                },
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
            cache: ValueCache::new(),
            upload_x: Vec::new(),
            upload_y: Vec::new(),
            idx_scratch: Vec::with_capacity(INDEX_CAPACITY as usize),
            readback_in_flight: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Submit the GPU render + readback copy for all visible line traces
    /// and return a [`ReadbackHandle`] the caller should `.read_image()` on
    /// a background thread. Returns `None` if a previous readback is still
    /// in flight, or there is nothing to draw.
    pub(super) fn render_to_gpu(
        &mut self,
        bounds: Bounds<Pixels>,
        view: PlotBounds,
        scale_factor: f32,
        traces: &[LineDraw<'_>],
    ) -> Option<ReadbackHandle> {
        if self.readback_in_flight.load(Ordering::Acquire) {
            return None;
        }

        let scale = scale_factor.max(1.0);
        let width = ((f32::from(bounds.size.width) * scale).round() as u32).max(1);
        let height = ((f32::from(bounds.size.height) * scale).round() as u32).max(1);
        if width == 0 || height == 0 || traces.is_empty() {
            return None;
        }

        if self
            .target
            .as_ref()
            .is_none_or(|t| t.width != width || t.height != height)
        {
            self.target = Some(RenderTarget::new(&self.ctx.device, width, height));
        }

        let handle: Option<ReadbackHandle> = {
            let LineRenderer {
                ctx,
                cache,
                upload_x,
                upload_y,
                idx_scratch,
                x_buf,
                y_buf,
                idx_buf,
                view_buf,
                line_buf,
                view_bg,
                line_bg,
                storage_bg,
                pipeline,
                target,
                ..
            } = self;
            let target = target.as_ref()?;

            idx_scratch.clear();
            let pixel_budget = width as usize;
            let mut plans: Vec<TracePlan> = Vec::with_capacity(traces.len());

            for trace in traces.iter() {
                let plan = plan_trace(
                    ctx,
                    cache,
                    upload_x,
                    upload_y,
                    idx_scratch,
                    x_buf,
                    y_buf,
                    trace,
                    view,
                    pixel_budget,
                );
                plans.push(plan);
            }

            if plans.iter().all(|p| p.spans.is_empty()) {
                return None;
            }

            let epoch_ns = cache.epoch_ns.unwrap_or(view.min_x as i64);
            let view_min_sec = ((view.min_x as i64 - epoch_ns) as f64 / NS_PER_SEC) as f32;
            let view_max_sec = ((view.max_x as i64 - epoch_ns) as f64 / NS_PER_SEC) as f32;
            let dx_sec = (view_max_sec - view_min_sec).max(1e-12);
            let dy = (view.max_y - view.min_y).max(1e-12);
            let scale_x = 2.0 / dx_sec;
            let scale_y = (2.0 / dy) as f32;
            let view_uniform = ViewUniform {
                scale: [scale_x, scale_y],
                offset: [
                    -1.0 - view_min_sec * scale_x,
                    -1.0 - (view.min_y as f32) * scale_y,
                ],
                viewport: [width as f32, height as f32],
                _pad: [0.0; 2],
            };
            ctx.queue
                .write_buffer(view_buf, 0, bytemuck::bytes_of(&view_uniform));
            ctx.queue
                .write_buffer(idx_buf, 0, bytemuck::cast_slice(idx_scratch));

            let mut any_drawn = false;
            for (trace, plan) in traces.iter().zip(plans.iter()) {
                if plan.spans.is_empty() {
                    continue;
                }
                let rgba = trace.color.to_rgb();
                let line_uniform = LineUniform {
                    color: [rgba.r, rgba.g, rgba.b, rgba.a],
                    line_width: trace.stroke_width * scale,
                    _pad: [0.0; 3],
                };
                ctx.queue
                    .write_buffer(line_buf, 0, bytemuck::bytes_of(&line_uniform));

                let load = if !any_drawn {
                    wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                } else {
                    wgpu::LoadOp::Load
                };
                let mut encoder =
                    ctx.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("line encoder"),
                        });
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("line pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &target.msaa_view,
                            depth_slice: None,
                            resolve_target: Some(&target.resolve_view),
                            ops: wgpu::Operations {
                                load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, &*view_bg, &[]);
                    pass.set_bind_group(1, &*line_bg, &[]);
                    pass.set_bind_group(2, &*storage_bg, &[]);
                    for span in &plan.spans {
                        pass.draw(0..4, span.instance_start..span.instance_end);
                    }
                }
                ctx.queue.submit(Some(encoder.finish()));
                any_drawn = true;
            }

            if !any_drawn {
                return None;
            }

            let mut encoder = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("line readback"),
                });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &target.resolve_texture,
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
                wgpu::Extent3d {
                    width: target.width,
                    height: target.height,
                    depth_or_array_layers: 1,
                },
            );
            ctx.queue.submit(Some(encoder.finish()));

            self.readback_in_flight.store(true, Ordering::Release);
            target
                .staging
                .slice(..)
                .map_async(wgpu::MapMode::Read, |_| {});

            Some(ReadbackHandle {
                ctx: ctx.clone(),
                staging: target.staging.clone(),
                width: target.width,
                height: target.height,
                padded_bytes_per_row: target.padded_bytes_per_row,
                in_flight: self.readback_in_flight.clone(),
            })
        };

        handle
    }
}

fn read_mapped_bytes(
    staging: &wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
) -> Option<Vec<u8>> {
    let slice = staging.slice(..);
    let unpadded_row = width as usize * 4;
    let padded_row = padded_bytes_per_row as usize;
    let mut out = Vec::with_capacity(unpadded_row * height as usize);
    let view = slice.get_mapped_range();
    for row in 0..height as usize {
        let start = row * padded_row;
        out.extend_from_slice(&view[start..start + unpadded_row]);
    }
    drop(view);
    Some(out)
}

/// One contiguous range of `idx_buf` instances belonging to a single trace.
struct DrawSpan {
    instance_start: u32,
    instance_end: u32,
}

struct TracePlan {
    spans: Vec<DrawSpan>,
}

/// Resolve the visible nodes for one trace, ensure each is resident in the
/// value cache, and emit decimated indices into `idx_scratch`. Returns the
/// list of `(start, end)` instance ranges to draw, one per visible chunk.
#[allow(clippy::too_many_arguments)]
fn plan_trace(
    ctx: &GpuContext,
    cache: &mut ValueCache,
    upload_x: &mut Vec<f32>,
    upload_y: &mut Vec<f32>,
    idx_scratch: &mut Vec<u32>,
    x_buf: &wgpu::Buffer,
    y_buf: &wgpu::Buffer,
    trace: &LineDraw<'_>,
    view: PlotBounds,
    pixel_budget: usize,
) -> TracePlan {
    let mut spans = Vec::new();
    if pixel_budget == 0 {
        return TracePlan { spans };
    }
    let start = Timestamp(view.min_x as i64);
    let end = Timestamp(view.max_x as i64);
    let Some(slice) = trace.component.time_series.get_range(start..end) else {
        return TracePlan { spans };
    };
    let nodes: Vec<_> = slice.as_iter().collect();
    let total: usize = nodes.iter().map(|n| n.timestamps().len()).sum();

    for node in nodes.iter().rev() {
        let visible = node.timestamps();
        let visible_len = visible.len();
        if visible_len == 0 {
            continue;
        }
        let Some(stride) = super::node_stride(visible_len, total, pixel_budget) else {
            continue;
        };
        let Some((chunk_offset, chunk_count)) = cache.ensure(
            &ctx.queue,
            x_buf,
            y_buf,
            upload_x,
            upload_y,
            trace.component_id,
            trace.element_index,
            node,
            &trace.component.schema,
        ) else {
            continue;
        };

        let full = node.full_timestamps();
        let slice_start_idx =
            unsafe { visible.as_ptr().offset_from(full.as_ptr()).max(0) as usize };

        let span_first = idx_scratch.len() as u32;
        let mut count_pushed: u32 = 0;
        let mut i = 0;
        while i < visible_len {
            let absolute = slice_start_idx + i;
            if (absolute as u32) >= chunk_count {
                break;
            }
            idx_scratch.push(chunk_offset + absolute as u32);
            count_pushed += 1;
            i += stride;
        }
        if count_pushed >= 2 {
            spans.push(DrawSpan {
                instance_start: span_first,
                instance_end: span_first + count_pushed - 1,
            });
        } else {
            idx_scratch.truncate(span_first as usize);
        }
    }

    TracePlan { spans }
}

/// Convert a slice of `Timestamp`s to `f32` seconds relative to `epoch_ns`.
fn convert_timestamps(timestamps: &[Timestamp], epoch_ns: i64, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(timestamps.len());
    for ts in timestamps {
        let delta_ns = ts.0 - epoch_ns;
        out.push((delta_ns as f64 / NS_PER_SEC) as f32);
    }
}

/// Convert `data[from..to]` for the given element index into f32 values.
fn convert_values(
    schema: &ComponentSchema,
    full_data: &[u8],
    element_index: usize,
    from: usize,
    to: usize,
    out: &mut Vec<f32>,
) {
    let elem_size = schema.size();
    out.clear();
    out.reserve(to - from);

    fn fill<T: super::PlotValue>(
        data: &[u8],
        from: usize,
        to: usize,
        elem_size: usize,
        elem_index: usize,
        out: &mut Vec<f32>,
    ) {
        for i in from..to {
            let v = super::read_value::<T>(data, i, elem_size, elem_index).unwrap_or(0.0);
            out.push(v as f32);
        }
    }

    match schema.prim_type {
        PrimType::F64 => fill::<f64>(full_data, from, to, elem_size, element_index, out),
        PrimType::F32 => fill::<f32>(full_data, from, to, elem_size, element_index, out),
        PrimType::I64 => fill::<i64>(full_data, from, to, elem_size, element_index, out),
        PrimType::I32 => fill::<i32>(full_data, from, to, elem_size, element_index, out),
        PrimType::I16 => fill::<i16>(full_data, from, to, elem_size, element_index, out),
        PrimType::I8 => fill::<i8>(full_data, from, to, elem_size, element_index, out),
        PrimType::U64 => fill::<u64>(full_data, from, to, elem_size, element_index, out),
        PrimType::U32 => fill::<u32>(full_data, from, to, elem_size, element_index, out),
        PrimType::U16 => fill::<u16>(full_data, from, to, elem_size, element_index, out),
        PrimType::U8 | PrimType::Bool => {
            fill::<u8>(full_data, from, to, elem_size, element_index, out)
        }
    }
}
