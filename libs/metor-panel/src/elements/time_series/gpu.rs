//! GPU-accelerated plot renderer.
//!
//! A single process-wide [`PlotGpu`] (a [`gpui::Global`]) owns the wgpu
//! pipelines, shared storage buffers, and a cross-plot [`ValueCache`].
//! Each call site — the `TimeSeriesPlot` panel, `Monitor` sparklines,
//! and the `ComponentTable` row sparklines — owns one lightweight
//! [`PlotRenderState`] that carries its per-instance render target,
//! the frame currently blitted on its canvas, and an "in-flight" flag.
//!
//! Control flow per frame:
//!
//! 1. Caller builds a `&[LineDraw]` from whatever traces it wants to
//!    render and calls [`PlotRenderState::render`] from inside its
//!    canvas prepaint closure.
//! 2. `render` lazy-inits the global, reuses or rebuilds the per-caller
//!    render target, submits the draw to wgpu, and returns a
//!    [`ReadbackHandle`] (`None` if a previous readback is still in
//!    flight or there's nothing to draw).
//! 3. The caller forwards the handle to [`ReadbackHandle::spawn_and_set`],
//!    which runs the staging-buffer read on a background worker, wraps
//!    the bytes into a `gpui::RenderImage`, and stores it back into the
//!    per-caller state via a pointer-to-field supplied by the caller.
//! 4. The caller's canvas paint closure blits `state.current_frame()`.

// `bytemuck::Pod`/`Zeroable` derives expand into helper items rustc can't
// see through, producing spurious dead-code warnings on the uniform
// structs whose fields are only read via `bytemuck::bytes_of`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytemuck::{Pod, Zeroable};
use gpui::{BorrowAppContext, Bounds, Context, Global, Hsla, Pixels, RenderImage};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::time_series::TimeSeriesNodeSlice;
use metor_db::{Component, ComponentSchema};
use metor_proto::types::{ComponentId, PrimType, Timestamp};
use offset_allocator::{Allocation, Allocator};
use smallvec::SmallVec;

use super::{PlotBounds, PlotStyle};
use crate::gpu_context::GpuContext;

const VALUE_CAPACITY: u32 = 1 << 22;
const VALUE_BUF_BYTES: u64 = VALUE_CAPACITY as u64 * 4;
const INDEX_CAPACITY: u32 = 1 << 18;
const INDEX_BUF_BYTES: u64 = INDEX_CAPACITY as u64 * 4;
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
const SAMPLE_COUNT: u32 = 4;
const NS_PER_SEC: f64 = 1.0e9;
const UNIFORM_ALIGN: u64 = 256;
const MAX_TRACES: usize = 64;

/// One trace's worth of data the renderer needs each frame. Callers
/// build a slice of these per paint.
pub(crate) struct LineDraw<'a> {
    pub component_id: ComponentId,
    pub component: &'a Component,
    pub element_index: usize,
    pub style: PlotStyle,
    pub color: Hsla,
    pub stroke_width: f32,
}

/// LOD level strides. LOD n stores every `LOD_STRIDES[n]`-th sample.
const LOD_STRIDES: [usize; 5] = [1, 4, 16, 64, 256];

/// Pick the coarsest LOD whose stride does not exceed `needed_stride`.
fn select_lod(needed_stride: usize) -> usize {
    for (i, &s) in LOD_STRIDES.iter().enumerate().rev() {
        if s <= needed_stride {
            return i;
        }
    }
    0
}

/// Stable identity for a cached chunk: one element column of one node at
/// a specific level of detail. Uniquely addresses any node across every
/// plot sharing the global cache.
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
struct ChunkKey {
    component_id: u64,
    element_index: u32,
    node_id: usize,
    lod: u32,
}

/// A node's worth of converted f32 samples currently resident in the
/// shared value buffers.
struct ResidentChunk {
    allocation: Allocation,
    capacity: u32,
    sample_count: u32,
}

/// Cross-plot offset-allocator cache mapping each visible node to a
/// region of the shared `x_buf` / `y_buf`. Uploaded chunks stay resident
/// until they either grow past their allocation or the allocator runs
/// out of space and evicts them.
struct ValueCache {
    allocator: Allocator,
    resident: HashMap<ChunkKey, ResidentChunk>,
    /// Reference epoch for the x axis, in nanoseconds. All cached x
    /// values are stored as `f32` seconds relative to this. Set lazily
    /// on first upload so the dynamic range stays small enough for `f32`
    /// to keep useful precision. Shared across plots; dashboards viewing
    /// a common "now" stay well within f32's range.
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

    /// Evict entries starting from the finest LOD level until `needed`
    /// slots are freed. Falls back to evicting everything if finer
    /// levels don't suffice.
    fn evict_for(&mut self, needed: u32) {
        let mut freed = 0u32;
        for target_lod in 0..LOD_STRIDES.len() as u32 {
            let keys: Vec<ChunkKey> = self
                .resident
                .keys()
                .filter(|k| k.lod == target_lod)
                .copied()
                .collect();
            for key in keys {
                if freed >= needed {
                    return;
                }
                if let Some(chunk) = self.resident.remove(&key) {
                    freed += chunk.capacity;
                    self.allocator.free(chunk.allocation);
                }
            }
        }
    }

    /// Ensure the node behind `slice` is resident at the given LOD level.
    /// Returns `(offset, decimated_sample_count)` into the shared value
    /// buffers.
    #[allow(clippy::too_many_arguments)]
    fn ensure_lod(
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
        lod: usize,
    ) -> Option<(u32, u32)> {
        let timestamps = slice.full_timestamps();
        let full_len = timestamps.len();
        if full_len == 0 {
            return None;
        }
        if self.epoch_ns.is_none() {
            self.epoch_ns = Some(timestamps[0].0);
        }
        let epoch = self.epoch_ns?;
        let lod_stride = LOD_STRIDES[lod];
        let decimated_len = full_len.div_ceil(lod_stride);

        let key = ChunkKey {
            component_id: component_id.0,
            element_index: element_index as u32,
            node_id: slice.node_id(),
            lod: lod as u32,
        };

        if let Some(chunk) = self.resident.get(&key) {
            if chunk.sample_count as usize >= decimated_len {
                return Some((chunk.allocation.offset, chunk.sample_count));
            }
            if (decimated_len as u32) > chunk.capacity {
                // Node grew past capacity — free and re-upload.
                let removed = self.resident.remove(&key).unwrap();
                self.allocator.free(removed.allocation);
            }
        }

        if !self.resident.contains_key(&key) {
            let want = (decimated_len as u32).next_power_of_two().max(16);
            let allocation = match self.allocator.allocate(want) {
                Some(a) => a,
                None => {
                    self.evict_for(want);
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
        if from < decimated_len {
            convert_timestamps_strided(
                timestamps,
                lod_stride,
                from,
                decimated_len,
                epoch,
                scratch_x,
            );
            convert_values_strided(
                schema,
                slice.full_data(),
                element_index,
                lod_stride,
                from,
                decimated_len,
                scratch_y,
            );
            let byte_offset = (chunk.allocation.offset + from as u32) as u64 * 4;
            queue.write_buffer(x_buf, byte_offset, bytemuck::cast_slice(scratch_x));
            queue.write_buffer(y_buf, byte_offset, bytemuck::cast_slice(scratch_y));
            chunk.sample_count = decimated_len as u32;
        }
        Some((chunk.allocation.offset, chunk.sample_count))
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

/// Per-caller off-screen target: MSAA + resolve + staging buffer sized
/// to the caller's current pixel bounds.
struct RenderTarget {
    width: u32,
    height: u32,
    msaa_view: wgpu::TextureView,
    resolve_texture: wgpu::Texture,
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
            label: Some("plot msaa"),
            size: extent,
            mip_level_count: 1,
            sample_count: SAMPLE_COUNT,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let resolve_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("plot resolve"),
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
            label: Some("plot staging"),
            size: padded_bytes_per_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            width,
            height,
            msaa_view,
            resolve_texture,
            resolve_view,
            staging,
            padded_bytes_per_row,
        }
    }
}

/// Handle returned by [`PlotRenderState::render`] when a new frame is
/// submitted. Call [`Self::spawn_and_set`] (or [`Self::read_image`]
/// directly) on a background worker to drive the readback.
pub(crate) struct ReadbackHandle {
    ctx: Arc<GpuContext>,
    staging: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    in_flight: Arc<AtomicBool>,
}

impl ReadbackHandle {
    /// Block the current thread until the GPU finishes, then read the
    /// mapped staging bytes and build a `RenderImage`. Call from a
    /// background thread (e.g. via `BackgroundExecutor::spawn`).
    pub(crate) fn read_image(self) -> Option<Arc<RenderImage>> {
        let _ = self.ctx.device.poll(wgpu::PollType::wait_indefinitely());
        let bytes = read_mapped_bytes(
            &self.staging,
            self.width,
            self.height,
            self.padded_bytes_per_row,
        );
        self.staging.unmap();
        self.in_flight.store(false, Ordering::Release);
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(self.width, self.height, bytes)?;
        let frames = SmallVec::from_elem(Frame::new(buffer), 1);
        Some(Arc::new(RenderImage::new(frames)))
    }

    /// Spawn a background worker that reads the frame back, wraps it
    /// into a `RenderImage`, and installs it via `access(t).set_frame()`.
    /// The caller supplies a function pointer pointing at whichever
    /// field holds its [`PlotRenderState`], so a single line at each
    /// call site replaces the bg-task + update + notify boilerplate.
    pub(crate) fn spawn_and_set<T: 'static>(
        self,
        cx: &mut Context<T>,
        access: fn(&mut T) -> &mut PlotRenderState,
    ) {
        cx.spawn(async move |this, cx| {
            let image = cx
                .background_executor()
                .spawn(async move { self.read_image() })
                .await;
            let Some(img) = image else {
                return;
            };
            let _ = this.update(cx, |t, cx| {
                access(t).set_frame(img);
                cx.notify();
            });
        })
        .detach();
    }
}

/// Process-wide GPU plot renderer. Lazy-initialized on the first
/// [`PlotRenderState::render`] call and stored as a [`gpui::Global`].
pub(super) struct PlotGpu {
    ctx: Arc<GpuContext>,
    line_pipeline: wgpu::RenderPipeline,
    scatter_pipeline: wgpu::RenderPipeline,
    bars_pipeline: wgpu::RenderPipeline,
    view_buf: wgpu::Buffer,
    view_bg: wgpu::BindGroup,
    line_buf: wgpu::Buffer,
    line_bg: wgpu::BindGroup,
    x_buf: wgpu::Buffer,
    y_buf: wgpu::Buffer,
    idx_buf: wgpu::Buffer,
    storage_bg: wgpu::BindGroup,
    cache: ValueCache,
    upload_x: Vec<f32>,
    upload_y: Vec<f32>,
    idx_scratch: Vec<u32>,
}

impl Global for PlotGpu {}

impl PlotGpu {
    fn try_new() -> Option<Self> {
        let ctx = GpuContext::get()?;
        let device = &ctx.device;

        let line_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("line.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("line.wgsl").into()),
        });
        let scatter_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scatter.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scatter.wgsl").into()),
        });
        let bars_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bars.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("bars.wgsl").into()),
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
                    has_dynamic_offset: true,
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

        let make_pipeline = |label: &str, shader: &wgpu::ShaderModule| -> wgpu::RenderPipeline {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
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
                    module: shader,
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
            })
        };

        let line_pipeline = make_pipeline("line pipeline", &line_shader);
        let scatter_pipeline = make_pipeline("scatter pipeline", &scatter_shader);
        let bars_pipeline = make_pipeline("bars pipeline", &bars_shader);

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
            size: UNIFORM_ALIGN * MAX_TRACES as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let line_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("line uniform bg"),
            layout: &line_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &line_buf,
                    offset: 0,
                    size: Some(std::num::NonZeroU64::new(UNIFORM_ALIGN).unwrap()),
                }),
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
            line_pipeline,
            scatter_pipeline,
            bars_pipeline,
            view_buf,
            view_bg,
            line_buf,
            line_bg,
            x_buf,
            y_buf,
            idx_buf,
            storage_bg,
            cache: ValueCache::new(),
            upload_x: Vec::new(),
            upload_y: Vec::new(),
            idx_scratch: Vec::with_capacity(INDEX_CAPACITY as usize),
        })
    }

    /// Submit the GPU render for one plot into the caller's
    /// [`RenderTarget`]. Returns `true` if any work was actually
    /// encoded and submitted (caller starts its readback); `false` if
    /// there was nothing to draw.
    fn submit(
        &mut self,
        target: &RenderTarget,
        view: PlotBounds,
        scale: f32,
        traces: &[LineDraw<'_>],
    ) -> bool {
        self.idx_scratch.clear();
        let pixel_budget = target.width as usize;

        let mut plans: Vec<TracePlan> = Vec::with_capacity(traces.len());
        for trace in traces {
            let plan = plan_trace(
                &self.ctx,
                &mut self.cache,
                &mut self.upload_x,
                &mut self.upload_y,
                &mut self.idx_scratch,
                &self.x_buf,
                &self.y_buf,
                trace,
                view,
                pixel_budget,
            );
            plans.push(plan);
        }
        if plans.iter().all(|p| p.spans.is_empty()) {
            return false;
        }

        let epoch_ns = self.cache.epoch_ns.unwrap_or(view.min_x as i64);
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
            viewport: [target.width as f32, target.height as f32],
            _pad: [0.0; 2],
        };
        self.ctx
            .queue
            .write_buffer(&self.view_buf, 0, bytemuck::bytes_of(&view_uniform));
        self.ctx
            .queue
            .write_buffer(&self.idx_buf, 0, bytemuck::cast_slice(&self.idx_scratch));

        // Pack all per-trace uniforms into the dynamic uniform buffer so
        // we can draw everything in a single submit.
        let mut live_traces: SmallVec<[(usize, PlotStyle, &TracePlan); 8]> = SmallVec::new();
        for (i, (trace, plan)) in traces.iter().zip(plans.iter()).enumerate() {
            if plan.spans.is_empty() || i >= MAX_TRACES {
                continue;
            }
            let rgba = trace.color.to_rgb();
            let lw = match trace.style {
                PlotStyle::Bar => {
                    let visible_count = plan
                        .spans
                        .iter()
                        .map(|s| s.instance_end - s.instance_start)
                        .sum::<u32>()
                        .max(1);
                    (target.width as f32 / visible_count as f32 * 0.4).clamp(scale, 20.0 * scale)
                }
                PlotStyle::Scatter => trace.stroke_width * scale * 3.0,
                PlotStyle::Line => trace.stroke_width * scale,
            };
            let uniform = LineUniform {
                color: [rgba.r, rgba.g, rgba.b, rgba.a],
                line_width: lw,
                _pad: [0.0; 3],
            };
            self.ctx.queue.write_buffer(
                &self.line_buf,
                i as u64 * UNIFORM_ALIGN,
                bytemuck::bytes_of(&uniform),
            );
            live_traces.push((i, trace.style, plan));
        }
        if live_traces.is_empty() {
            return false;
        }

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("plot encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("plot pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.msaa_view,
                    depth_slice: None,
                    resolve_target: Some(&target.resolve_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.view_bg, &[]);
            pass.set_bind_group(2, &self.storage_bg, &[]);

            let mut current_pipeline: Option<PlotStyle> = None;
            for &(slot, style, plan) in &live_traces {
                if current_pipeline != Some(style) {
                    pass.set_pipeline(match style {
                        PlotStyle::Line => &self.line_pipeline,
                        PlotStyle::Scatter => &self.scatter_pipeline,
                        PlotStyle::Bar => &self.bars_pipeline,
                    });
                    current_pipeline = Some(style);
                }
                let offset = (slot as u64 * UNIFORM_ALIGN) as u32;
                pass.set_bind_group(1, &self.line_bg, &[offset]);
                for span in &plan.spans {
                    pass.draw(0..4, span.instance_start..span.instance_end);
                }
            }
        }

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
        self.ctx.queue.submit(Some(encoder.finish()));
        true
    }
}

/// Per-caller render state: owns one [`RenderTarget`] sized to the
/// caller's bounds, the frame currently blitted to its canvas, and the
/// in-flight flag that back-pressures submissions while a readback is
/// still outstanding.
pub(crate) struct PlotRenderState {
    target: Option<RenderTarget>,
    current_frame: Option<Arc<RenderImage>>,
    pending_release: Option<Arc<RenderImage>>,
    in_flight: Arc<AtomicBool>,
}

impl Default for PlotRenderState {
    fn default() -> Self {
        Self {
            target: None,
            current_frame: None,
            pending_release: None,
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl PlotRenderState {
    /// Submit a fresh render for this plot. Lazy-inits [`PlotGpu`] the
    /// first time it's called; returns `None` if GPU context is
    /// unavailable, the caller has a readback still in flight, the
    /// bounds are zero, there are no visible traces, or nothing ends
    /// up being drawn after decimation.
    pub(crate) fn render(
        &mut self,
        cx: &mut gpui::App,
        bounds: Bounds<Pixels>,
        scale_factor: f32,
        view: PlotBounds,
        traces: &[LineDraw<'_>],
    ) -> Option<ReadbackHandle> {
        if self.in_flight.load(Ordering::Acquire) {
            return None;
        }
        let scale = scale_factor.max(1.0);
        let width = ((f32::from(bounds.size.width) * scale).round() as u32).max(1);
        let height = ((f32::from(bounds.size.height) * scale).round() as u32).max(1);
        if traces.is_empty() {
            return None;
        }

        if !cx.has_global::<PlotGpu>() {
            let gpu = PlotGpu::try_new()?;
            cx.set_global(gpu);
        }
        cx.update_global::<PlotGpu, _>(|gpu, _| {
            if self
                .target
                .as_ref()
                .is_none_or(|t| t.width != width || t.height != height)
            {
                self.target = Some(RenderTarget::new(&gpu.ctx.device, width, height));
            }
            let target = self.target.as_ref()?;
            if !gpu.submit(target, view, scale, traces) {
                return None;
            }
            self.in_flight.store(true, Ordering::Release);
            target
                .staging
                .slice(..)
                .map_async(wgpu::MapMode::Read, |_| {});
            Some(ReadbackHandle {
                ctx: gpu.ctx.clone(),
                staging: target.staging.clone(),
                width: target.width,
                height: target.height,
                padded_bytes_per_row: target.padded_bytes_per_row,
                in_flight: self.in_flight.clone(),
            })
        })
    }

    /// Replace the current frame with a freshly-read `RenderImage`,
    /// parking the old frame for release from gpui's sprite atlas on
    /// the next paint.
    pub(crate) fn set_frame(&mut self, image: Arc<RenderImage>) {
        self.pending_release = self.current_frame.replace(image);
    }

    /// Clone the current frame handle for blitting via `paint_image`.
    pub(crate) fn current_frame(&self) -> Option<Arc<RenderImage>> {
        self.current_frame.clone()
    }

    /// Take the frame waiting to be released from gpui's sprite atlas.
    /// The caller passes each entry to `window.drop_image` inside the
    /// prepaint closure that has `&mut Window` access.
    pub(crate) fn take_pending_release(&mut self) -> Option<Arc<RenderImage>> {
        self.pending_release.take()
    }
}

fn read_mapped_bytes(
    staging: &wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
) -> Vec<u8> {
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
    out
}

/// One contiguous range of `idx_buf` instances belonging to a single
/// trace.
struct DrawSpan {
    instance_start: u32,
    instance_end: u32,
}

struct TracePlan {
    spans: Vec<DrawSpan>,
}

/// Resolve the visible nodes for one trace, ensure each is resident in
/// the value cache, and emit decimated indices into `idx_scratch`.
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
        let Some(stride) = node_stride(visible_len, total, pixel_budget) else {
            continue;
        };

        let lod = select_lod(stride);
        let lod_stride = LOD_STRIDES[lod];

        let Some((chunk_offset, decimated_count)) = cache.ensure_lod(
            &ctx.queue,
            x_buf,
            y_buf,
            upload_x,
            upload_y,
            trace.component_id,
            trace.element_index,
            node,
            &trace.component.schema,
            lod,
        ) else {
            continue;
        };

        let full = node.full_timestamps();
        let slice_start_idx =
            unsafe { visible.as_ptr().offset_from(full.as_ptr()).max(0) as usize };
        // Pull one extra sample past each end of the visible range so
        // the segments that cross the viewport boundary are drawn (and
        // clipped by the GPU) instead of being dropped.
        let ext_start_idx = slice_start_idx.saturating_sub(1);
        let ext_end_idx = (slice_start_idx + visible_len + 1).min(full.len());

        let lod_start = ext_start_idx / lod_stride;
        let lod_end = ext_end_idx
            .div_ceil(lod_stride)
            .min(decimated_count as usize);
        let residual_stride = (stride / lod_stride).max(1);

        let is_line = matches!(trace.style, PlotStyle::Line);
        let span_first = idx_scratch.len() as u32;
        let mut count_pushed: u32 = 0;
        let mut i = lod_start;
        while i < lod_end {
            idx_scratch.push(chunk_offset + i as u32);
            count_pushed += 1;
            i += residual_stride;
        }
        let min_for_draw = if is_line { 2u32 } else { 1 };
        if count_pushed >= min_for_draw {
            let instance_end = if is_line {
                span_first + count_pushed - 1
            } else {
                span_first + count_pushed
            };
            spans.push(DrawSpan {
                instance_start: span_first,
                instance_end,
            });
        } else {
            idx_scratch.truncate(span_first as usize);
        }
    }

    TracePlan { spans }
}

/// Compute per-node stride for two-level decimation. Returns the step
/// between sample indices within a node, or `None` to skip it entirely.
fn node_stride(node_len: usize, total: usize, pixel_budget: usize) -> Option<usize> {
    if node_len == 0 || total == 0 || pixel_budget == 0 {
        return None;
    }
    let node_budget = ((node_len as f64 / total as f64) * pixel_budget as f64).ceil() as usize;
    if node_budget == 0 {
        return None;
    }
    Some((node_len / node_budget).max(1))
}

/// Primitive types that can be read from a byte buffer and converted to
/// f64. Shared between `convert_values_strided` and the upstream trace
/// planner.
pub(super) trait PlotValue: zerocopy::FromBytes + Copy + Sized + 'static {
    fn to_f64(self) -> f64;
}

macro_rules! impl_plot_value {
    ($($ty:ty => $conv:expr),* $(,)?) => {
        $(impl PlotValue for $ty {
            #[inline(always)]
            fn to_f64(self) -> f64 { $conv(self) }
        })*
    };
}

impl_plot_value! {
    f64 => |x| x,
    f32 => |x: f32| x as f64,
    i8  => |x: i8| x as f64,
    i16 => |x: i16| x as f64,
    i32 => |x: i32| x as f64,
    i64 => |x: i64| x as f64,
    u8  => |x: u8| x as f64,
    u16 => |x: u16| x as f64,
    u32 => |x: u32| x as f64,
    u64 => |x: u64| x as f64,
}

/// Read a single element value directly from a raw data buffer. Bool
/// isn't `FromBytes` (not all bit patterns valid), so callers of the
/// `PrimType::Bool` branch route through `u8`.
#[inline(always)]
fn read_value<T: PlotValue>(
    data: &[u8],
    sample_index: usize,
    elem_size: usize,
    elem_index: usize,
) -> Option<f64> {
    let offset = sample_index * elem_size + elem_index * size_of::<T>();
    let buf = data.get(offset..offset + size_of::<T>())?;
    T::read_from_bytes(buf).ok().map(|v| v.to_f64())
}

/// Convert every `lod_stride`-th timestamp in decimated index range
/// `[from..to)` to `f32` seconds relative to `epoch_ns`.
fn convert_timestamps_strided(
    timestamps: &[Timestamp],
    lod_stride: usize,
    from: usize,
    to: usize,
    epoch_ns: i64,
    out: &mut Vec<f32>,
) {
    out.clear();
    out.reserve(to - from);
    for decimated_i in from..to {
        let src = (decimated_i * lod_stride).min(timestamps.len().saturating_sub(1));
        let delta_ns = timestamps[src].0 - epoch_ns;
        out.push((delta_ns as f64 / NS_PER_SEC) as f32);
    }
}

/// Convert every `lod_stride`-th data sample in decimated index range
/// `[from..to)` for the given element index into f32 values.
fn convert_values_strided(
    schema: &ComponentSchema,
    full_data: &[u8],
    element_index: usize,
    lod_stride: usize,
    from: usize,
    to: usize,
    out: &mut Vec<f32>,
) {
    let elem_size = schema.size();
    let max_sample = if elem_size == 0 {
        0
    } else {
        full_data.len() / elem_size
    };
    out.clear();
    out.reserve(to - from);

    fn fill<T: PlotValue>(
        data: &[u8],
        lod_stride: usize,
        from: usize,
        to: usize,
        max_sample: usize,
        elem_size: usize,
        elem_index: usize,
        out: &mut Vec<f32>,
    ) {
        for decimated_i in from..to {
            let src = (decimated_i * lod_stride).min(max_sample.saturating_sub(1));
            let v = read_value::<T>(data, src, elem_size, elem_index).unwrap_or(0.0);
            out.push(v as f32);
        }
    }

    match schema.prim_type {
        PrimType::F64 => fill::<f64>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::F32 => fill::<f32>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::I64 => fill::<i64>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::I32 => fill::<i32>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::I16 => fill::<i16>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::I8 => fill::<i8>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::U64 => fill::<u64>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::U32 => fill::<u32>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::U16 => fill::<u16>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
        PrimType::U8 | PrimType::Bool => fill::<u8>(
            full_data,
            lod_stride,
            from,
            to,
            max_sample,
            elem_size,
            element_index,
            out,
        ),
    }
}
