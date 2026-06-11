//! GPU pipeline shared by every plot in the panel.
//!
//! A single process-wide [`PlotGpu`] global owns the wgpu pipelines and
//! shared storage buffers. Each caller holds a lightweight
//! [`PlotRenderState`] carrying its own off-screen target and the frame
//! currently blitted to its canvas.
//!
//! The pipeline operates on generic `(x, y)` data-space pairs — there is no
//! intrinsic notion of time. Time-series and XY-plot callers both build
//! `LineDraw`s whose `x`/`y` axes are [`AxisSource`]s; the time-series mode
//! sets `x = AxisSource::Timestamps` so the materializer reads timestamps
//! out of the same node carrying the Y-axis values.
//!
//! Frame lifecycle:
//! 1. Caller passes a `&[LineDraw]` to [`PlotRenderState::render`] from
//!    inside a canvas prepaint closure.
//! 2. `render` resizes or allocates the per-caller target, submits the
//!    draw, and returns a [`ReadbackHandle`]. `None` when a previous
//!    readback is still in flight or the scene is empty.
//! 3. The caller passes the handle to [`ReadbackHandle::spawn_and_set`],
//!    which reads the staging buffer on a background worker, builds a
//!    `RenderImage`, and installs it back via a field accessor.
//! 4. The paint closure blits `state.current_frame()`.

// Uniform fields are only read via `bytemuck::bytes_of`, which the
// dead-code lint can't see through the Pod/Zeroable derive expansions.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytemuck::{Pod, Zeroable};
use gpui::{BorrowAppContext, Bounds, Context, Global, Hsla, Pixels, RenderImage};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::time_series::TimeSeriesNodeSlice;
use metor_db::{Component, ComponentSchema};
use metor_proto::types::{ComponentId, PrimType, Timestamp};
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

/// Where one axis of a trace gets its values.
///
/// `Timestamps` reads from a node's `Timestamp` index; the materializer
/// scales nanoseconds to seconds so f32 storage retains precision over
/// long durations. `Element` reads one element of one component schema's
/// raw byte buffer at its native scale.
///
/// `LatestSampleIndex` and `LatestSampleElements` source from the *interior*
/// of one sample (the latest), iterating across element offsets within that
/// single sample rather than walking the time-series. Used by list plots to
/// render an FFT-style vector as index-vs-value.
#[derive(Clone, Copy)]
pub(crate) enum AxisSource<'a> {
    Timestamps,
    Element {
        component_id: ComponentId,
        component: &'a Component,
        element_index: usize,
    },
    LatestSampleIndex {
        len: usize,
    },
    LatestSampleElements {
        component_id: ComponentId,
        component: &'a Component,
        len: usize,
    },
    /// Pre-aggregated min/max envelope bins fetched from a remote, sorted
    /// by timestamp. Renders as a zigzag through the line pipeline: two
    /// points per bin, so every column spans its bin's extremes — the
    /// same geometry min-max decimation produces from raw samples.
    EnvelopeBins {
        bins: &'a [metor_db::remote::EnvelopeBin],
    },
}

impl<'a> AxisSource<'a> {
    /// Conversion factor applied during materialization so the GPU sees a
    /// numerically stable f32 even when the source range is huge (e.g.,
    /// nanosecond timestamps).
    fn scale(&self) -> f64 {
        match self {
            AxisSource::Timestamps => 1.0 / NS_PER_SEC,
            AxisSource::Element { .. }
            | AxisSource::LatestSampleIndex { .. }
            | AxisSource::LatestSampleElements { .. }
            | AxisSource::EnvelopeBins { .. } => 1.0,
        }
    }

    fn component(&self) -> Option<&'a Component> {
        match self {
            AxisSource::Timestamps
            | AxisSource::LatestSampleIndex { .. }
            | AxisSource::EnvelopeBins { .. } => None,
            AxisSource::Element { component, .. }
            | AxisSource::LatestSampleElements { component, .. } => Some(component),
        }
    }
}

/// Draw inputs for one trace; callers build a slice of these per paint.
///
/// `y_min`/`y_max` are the data-space Y range of the trace's axis. The
/// materializer normalizes each Y sample into `[0,1]` against this range, so
/// the GPU's view uniform maps a fixed `0..1` regardless of axis — that's
/// what lets several axes with different scales share one render pass.
pub(crate) struct LineDraw<'a> {
    pub x: AxisSource<'a>,
    pub y: AxisSource<'a>,
    pub y_min: f64,
    pub y_max: f64,
    pub style: PlotStyle,
    pub color: Hsla,
    pub stroke_width: f32,
    /// Narrow the time-range cull below the view, in raw X units. Lets a
    /// trace render twice in one frame without overlap — an envelope for
    /// summarized history plus raw samples clipped to the live tail.
    pub x_clip: Option<(f64, f64)>,
}

/// Snap a decimation stride to the nearest power of 4.
///
/// Power-of-4 quantization keeps the sampled index set stable across small
/// zoom deltas (no "crawling" of rendered points), and scales the upload
/// unboundedly with data volume so extreme zoom-outs don't over-upload
/// the way a fixed LOD cap would.
fn quantize_stride(needed_stride: usize) -> usize {
    let n = needed_stride.max(1);
    let log2 = n.ilog2();
    let floor_p = 1usize << (log2 & !1);
    // `2 * floor_p` is the geometric midpoint between `floor_p` and `4 * floor_p`.
    if n >= floor_p * 2 {
        floor_p * 4
    } else {
        floor_p
    }
}

/// Per-frame bump allocator over the shared x/y storage buffers.
///
/// Cross-frame reuse is intentional dead weight here: decimation keeps each
/// trace's upload at ~`pixel_budget` samples regardless of source size, so
/// re-upload is cheap and we sidestep the aliasing problems a shared LRU
/// cache would have across traces.
struct ValueCache {
    /// Next free slot in samples (×4 bytes for the byte offset). Reset at
    /// the start of every `submit`.
    cursor: u32,
    /// Per-frame epoch for the X axis in raw plot-space units (nanoseconds
    /// for timestamps, native units for component elements).
    ///
    /// Anchored to the view's left edge each frame so `f32` precision is
    /// spent on the visible window rather than on the distance back to
    /// the oldest sample.
    epoch_x: f64,
    /// An upload this frame was clamped by buffer capacity; the rendered
    /// frame is missing samples and callers may want to surface that.
    truncated: bool,
}

impl ValueCache {
    fn new() -> Self {
        Self {
            cursor: 0,
            epoch_x: 0.0,
            truncated: false,
        }
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

/// Off-screen render target owned by one caller.
///
/// Wraps the MSAA color attachment, its resolve texture, and a
/// host-readable staging buffer, all sized to the caller's canvas.
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

/// Cookie representing an in-flight staging buffer readback.
///
/// Drive it to completion via [`ReadbackHandle::spawn_and_set`] on the
/// gpui side or [`ReadbackHandle::read_image`] directly on a worker.
pub(crate) struct ReadbackHandle {
    ctx: Arc<GpuContext>,
    staging: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    in_flight: Arc<AtomicBool>,
    /// Whether this frame's uploads were clamped by buffer capacity.
    /// Travels with the frame so the degraded badge always describes the
    /// image actually on screen.
    truncated: bool,
}

impl ReadbackHandle {
    /// Block until GPU work finishes, unmap the staging buffer, and wrap
    /// the pixels into a `RenderImage`. Intended for background workers.
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

    /// Run the readback on the background executor and install the
    /// resulting frame via `access(t).set_frame()`.
    ///
    /// `access` lets this helper reach whichever field the caller stores
    /// its [`PlotRenderState`] in, replacing a block of task/update/notify
    /// boilerplate at every call site.
    pub(crate) fn spawn_and_set<T: 'static>(
        self,
        cx: &mut Context<T>,
        access: fn(&mut T) -> &mut PlotRenderState,
    ) {
        cx.spawn(async move |this, cx| {
            let truncated = self.truncated;
            let image = cx
                .background_executor()
                .spawn(async move { self.read_image() })
                .await;
            let Some(img) = image else {
                return;
            };
            let _ = this.update(cx, |t, cx| {
                access(t).set_frame(img, truncated);
                cx.notify();
            });
        })
        .detach();
    }
}

/// Global owning the wgpu pipelines and shared storage buffers.
///
/// Stored as a [`gpui::Global`] and lazy-initialized on the first
/// [`PlotRenderState::render`] call.
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
    select_scratch: Vec<u32>,
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
            select_scratch: Vec::new(),
            idx_scratch: Vec::with_capacity(INDEX_CAPACITY as usize),
        })
    }

    /// Encode and submit one plot's frame into `target`.
    ///
    /// Returns `true` when real work was submitted (the caller starts a
    /// readback), `false` when every trace decimated to nothing.
    fn submit(
        &mut self,
        target: &RenderTarget,
        view: PlotBounds,
        scale: f32,
        traces: &[LineDraw<'_>],
    ) -> bool {
        self.idx_scratch.clear();
        self.cache.cursor = 0;
        self.cache.epoch_x = view.min_x;
        self.cache.truncated = false;
        let pixel_budget = target.width as usize;

        let mut plans: Vec<TracePlan> = Vec::with_capacity(traces.len());
        for trace in traces {
            // Per-trace Y normalization: map [y_min, y_max] onto [0, 1].
            let y_epoch = trace.y_min;
            let y_scale = 1.0 / (trace.y_max - trace.y_min).max(1e-12);
            let plan = plan_trace(
                &self.ctx,
                &mut self.cache,
                &mut self.upload_x,
                &mut self.upload_y,
                &mut self.select_scratch,
                &mut self.idx_scratch,
                &self.x_buf,
                &self.y_buf,
                trace,
                view,
                y_epoch,
                y_scale,
                pixel_budget,
            );
            plans.push(plan);
        }
        if self.cache.truncated {
            tracing::debug!(
                target: "plot::capacity",
                uploaded = self.cache.cursor,
                indices = self.idx_scratch.len(),
                "plot uploads clamped to buffer capacity"
            );
        }
        if plans.iter().all(|p| p.spans.is_empty()) {
            return false;
        }

        // X axis uniform space: shifted by epoch and scaled by the X
        // source's scale factor (1.0 for element values, 1e-9 for ns
        // timestamps). The `x_scale_factor` of the FIRST trace whose
        // source is well-defined drives the view uniform; all real-world
        // call sites compose homogeneous traces (all time-series xor all
        // XY), so picking the first is sufficient.
        let epoch_x = self.cache.epoch_x;
        let x_scale_factor = traces
            .first()
            .map(|t| t.x.scale())
            .unwrap_or(1.0 / NS_PER_SEC);
        let view_min_x = ((view.min_x - epoch_x) * x_scale_factor) as f32;
        let view_max_x = ((view.max_x - epoch_x) * x_scale_factor) as f32;
        let dx = (view_max_x - view_min_x).max(1e-12);
        let scale_x = 2.0 / dx;
        // Y arrives pre-normalized to [0,1] per trace (see `LineDraw`), so the
        // view uniform maps that fixed range to clip space [-1,1] for every
        // axis: scale 2, offset -1.
        let scale_y = 2.0_f32;
        let view_uniform = ViewUniform {
            scale: [scale_x, scale_y],
            offset: [-1.0 - view_min_x * scale_x, -1.0],
            viewport: [target.width as f32, target.height as f32],
            _pad: [0.0; 2],
        };
        self.ctx
            .queue
            .write_buffer(&self.view_buf, 0, bytemuck::bytes_of(&view_uniform));
        self.ctx
            .queue
            .write_buffer(&self.idx_buf, 0, bytemuck::cast_slice(&self.idx_scratch));

        // Pack every trace's uniform into one dynamic-offset buffer so
        // the whole frame goes in a single submit.
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

/// Caller-local render state: target, blitted frame, and the in-flight
/// flag that back-pressures submissions while a readback is outstanding.
pub(crate) struct PlotRenderState {
    target: Option<RenderTarget>,
    current_frame: Option<Arc<RenderImage>>,
    pending_release: Option<Arc<RenderImage>>,
    in_flight: Arc<AtomicBool>,
    degraded: bool,
}

impl Default for PlotRenderState {
    fn default() -> Self {
        Self {
            target: None,
            current_frame: None,
            pending_release: None,
            in_flight: Arc::new(AtomicBool::new(false)),
            degraded: false,
        }
    }
}

impl PlotRenderState {
    /// Submit a frame for this plot.
    ///
    /// Returns `None` when a prior readback is still in flight, the GPU
    /// context is unavailable, the bounds are degenerate, or every trace
    /// decimated to nothing. [`PlotGpu`] is lazy-initialized on the first
    /// call that reaches wgpu.
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
            self.degraded = false;
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
                truncated: gpu.cache.truncated,
            })
        })
    }

    /// Install a newly-read frame and park the previous one for release.
    pub(crate) fn set_frame(&mut self, image: Arc<RenderImage>, truncated: bool) {
        self.pending_release = self.current_frame.replace(image);
        self.degraded = truncated;
    }

    pub(crate) fn current_frame(&self) -> Option<Arc<RenderImage>> {
        self.current_frame.clone()
    }

    /// True when the displayed frame had uploads clamped by buffer
    /// capacity — the plot is showing fewer samples than the view holds.
    pub(crate) fn degraded(&self) -> bool {
        self.degraded
    }

    /// Take the previous frame so the caller can pass it to
    /// `window.drop_image`, which needs `&mut Window` access only
    /// available inside the prepaint closure.
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

/// Contiguous run of instance indices that belong to one trace.
struct DrawSpan {
    instance_start: u32,
    instance_end: u32,
}

struct TracePlan {
    spans: Vec<DrawSpan>,
}

/// View over one node, decoupled from `TimeSeriesNodeSlice` so the same
/// upload helper can serve time-series traces (which carry a view-culled
/// slice) and XY traces (which walk full nodes, possibly capped to the
/// shorter of the X/Y component's joint length).
///
/// Owns its underlying slice (cheap: an `Arc` plus a range) so the
/// computed start/end indices don't depend on borrowing across iterators.
struct NodeView {
    slice: TimeSeriesNodeSlice,
    /// Index into `slice.full_timestamps()` where the visible window begins.
    visible_start: usize,
    /// Number of samples in the visible window (already capped where applicable).
    visible_len: usize,
}

impl NodeView {
    fn from_slice(slice: TimeSeriesNodeSlice) -> Self {
        let visible = slice.timestamps();
        let full = slice.full_timestamps();
        let visible_start = if visible.is_empty() {
            0
        } else {
            unsafe { visible.as_ptr().offset_from(full.as_ptr()).max(0) as usize }
        };
        let visible_len = visible.len();
        Self {
            slice,
            visible_start,
            visible_len,
        }
    }

    /// Build a view over a full-extent slice, capping the visible window
    /// at `cap` samples. Used by XY traces where the X and Y components'
    /// node lengths must be paired at `min(len_x, len_y)`.
    fn from_full_slice_capped(slice: TimeSeriesNodeSlice, cap: usize) -> Self {
        let visible_len = slice.timestamps().len().min(cap);
        Self {
            slice,
            visible_start: 0,
            visible_len,
        }
    }

    fn full_timestamps(&self) -> &[Timestamp] {
        self.slice.full_timestamps()
    }

    fn full_data(&self) -> &[u8] {
        self.slice.full_data()
    }

    fn visible_start(&self) -> usize {
        self.visible_start
    }

    fn visible_end(&self) -> usize {
        self.visible_start + self.visible_len
    }

    fn visible_len(&self) -> usize {
        self.visible_len
    }
}

/// Walk one trace's visible nodes, decimate, upload to the value cache,
/// and emit draw spans referenced by `idx_scratch`.
#[allow(clippy::too_many_arguments)]
fn plan_trace(
    ctx: &GpuContext,
    cache: &mut ValueCache,
    upload_x: &mut Vec<f32>,
    upload_y: &mut Vec<f32>,
    select_scratch: &mut Vec<u32>,
    idx_scratch: &mut Vec<u32>,
    x_buf: &wgpu::Buffer,
    y_buf: &wgpu::Buffer,
    trace: &LineDraw<'_>,
    view: PlotBounds,
    y_epoch: f64,
    y_scale: f64,
    pixel_budget: usize,
) -> TracePlan {
    let mut spans = Vec::new();
    if pixel_budget == 0 {
        return TracePlan { spans };
    }

    // List-plot path: read one latest sample's vector elements rather
    // than walking nodes over time. Bypasses the NodePair pipeline since
    // the source byte layout is a single sample of `len * prim_size`
    // bytes — striding by element offset, not sample offset.
    if let (
        AxisSource::LatestSampleIndex { len: x_len },
        AxisSource::LatestSampleElements {
            component,
            len: y_len,
            ..
        },
    ) = (&trace.x, &trace.y)
    {
        let len = (*x_len).min(*y_len);
        return plan_list_trace(
            ctx,
            cache,
            upload_x,
            upload_y,
            idx_scratch,
            x_buf,
            y_buf,
            component,
            len,
            trace.style,
            y_epoch,
            y_scale,
            pixel_budget,
        );
    }

    if let AxisSource::EnvelopeBins { bins } = &trace.y {
        return plan_envelope_trace(
            ctx,
            cache,
            upload_x,
            upload_y,
            idx_scratch,
            x_buf,
            y_buf,
            bins,
            view,
            y_epoch,
            y_scale,
        );
    }

    let (cull_min_x, cull_max_x) = match trace.x_clip {
        Some((lo, hi)) => (lo.max(view.min_x), hi.min(view.max_x)),
        None => (view.min_x, view.max_x),
    };
    if cull_max_x <= cull_min_x {
        return TracePlan { spans };
    }

    // Build the per-axis node lists. Time-series uses time-range culling
    // on the Y axis's component; XY pairs nodes by stack index across two
    // components.
    let pairs: Vec<NodePair> = match (&trace.x, &trace.y) {
        (AxisSource::Timestamps, AxisSource::Element { component, .. }) => {
            let start = Timestamp(cull_min_x as i64);
            let end = Timestamp(cull_max_x as i64);
            let Some(slice) = component.time_series.get_range(start..end) else {
                return TracePlan { spans };
            };
            slice
                .as_iter()
                .map(|node| NodePair {
                    x: NodeView::from_slice(node.clone()),
                    y: NodeView::from_slice(node),
                })
                .collect()
        }
        (
            AxisSource::Element {
                component: comp_x, ..
            },
            AxisSource::Element {
                component: comp_y, ..
            },
        ) => {
            // AtomicStack iterates newest-first; collect both, pair by
            // stack index so identical-cadence components line up at the
            // node level. Mismatched lengths truncate to the shorter list
            // — this is the user-stated "assume samples are aligned"
            // mode; resampling is a follow-up.
            let xs: Vec<_> = comp_x.time_series.iter_node_slices().collect();
            let ys: Vec<_> = comp_y.time_series.iter_node_slices().collect();
            let n = xs.len().min(ys.len());
            xs.into_iter()
                .zip(ys)
                .take(n)
                .map(|(xn, yn)| {
                    let cap = xn.timestamps().len().min(yn.timestamps().len());
                    NodePair {
                        x: NodeView::from_full_slice_capped(xn, cap),
                        y: NodeView::from_full_slice_capped(yn, cap),
                    }
                })
                .collect()
        }
        _ => return TracePlan { spans },
    };

    let total: usize = pairs.iter().map(|p| p.y.visible_len()).sum();
    if total == 0 {
        return TracePlan { spans };
    }

    for pair in pairs.iter().rev() {
        let visible_len = pair.y.visible_len();
        if visible_len == 0 {
            continue;
        }
        let Some(stride) = node_stride(visible_len, total, pixel_budget) else {
            continue;
        };
        let upload_stride = quantize_stride(stride);

        // Extend one sample past each boundary so segments crossing the
        // edge are drawn and clipped by the GPU rather than dropped.
        // Only meaningful for view-culled time-series traces. For XY,
        // `visible_end` is `min(x_node_len, y_node_len)` and the +1
        // would invent a cross-component pair `(X[last_x], Y[cap])`
        // when nodes have unequal fill rates (24-byte Vec3 seals at
        // ~1.4M while paired Scalar seals at 4M).
        let extend_boundaries = matches!((&trace.x, &trace.y), (AxisSource::Timestamps, _),);
        let (ext_start_idx, ext_end_idx) = if extend_boundaries {
            (
                pair.y.visible_start().saturating_sub(1),
                (pair.y.visible_end() + 1).min(pair.y.full_timestamps().len()),
            )
        } else {
            (pair.y.visible_start(), pair.y.visible_end())
        };

        let lod_start = ext_start_idx / upload_stride;
        let full_decimated = pair.y.full_timestamps().len().div_ceil(upload_stride);
        let lod_end = ext_end_idx.div_ceil(upload_stride).min(full_decimated);

        let Some((chunk_offset, count)) = upload_pair(
            ctx,
            cache,
            upload_x,
            upload_y,
            select_scratch,
            x_buf,
            y_buf,
            &pair.x,
            &pair.y,
            &trace.x,
            &trace.y,
            y_epoch,
            y_scale,
            upload_stride,
            lod_start,
            lod_end,
        ) else {
            continue;
        };

        let is_line = matches!(trace.style, PlotStyle::Line);
        let span_first = idx_scratch.len() as u32;
        // Clamp to the index buffer: overflowing it would fail wgpu
        // validation and silently drop the whole frame.
        let index_available = (INDEX_CAPACITY as usize).saturating_sub(idx_scratch.len()) as u32;
        let count = if count > index_available {
            cache.truncated = true;
            index_available
        } else {
            count
        };
        let mut count_pushed: u32 = 0;
        for j in 0..count {
            idx_scratch.push(chunk_offset + j);
            count_pushed += 1;
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

struct NodePair {
    x: NodeView,
    y: NodeView,
}

/// Envelope path: two points per bin, `(ts, min)` then `(ts, max)`, drawn
/// as one zigzag line so each column spans its bin's extremes. Bypasses
/// the NodePair pipeline — bins are already decimated server-side and a
/// year is a few thousand of them. NaN bins (no finite sample) split the
/// line, so coverage holes render as holes rather than interpolating
/// across.
#[allow(clippy::too_many_arguments)]
fn plan_envelope_trace(
    ctx: &GpuContext,
    cache: &mut ValueCache,
    upload_x: &mut Vec<f32>,
    upload_y: &mut Vec<f32>,
    idx_scratch: &mut Vec<u32>,
    x_buf: &wgpu::Buffer,
    y_buf: &wgpu::Buffer,
    bins: &[metor_db::remote::EnvelopeBin],
    view: PlotBounds,
    y_epoch: f64,
    y_scale: f64,
) -> TracePlan {
    let mut spans = Vec::new();
    // One bin of margin each side so edge segments clip instead of pop.
    let lo = bins
        .partition_point(|b| (b.ts.0 as f64) < view.min_x)
        .saturating_sub(1);
    let hi = (bins.partition_point(|b| (b.ts.0 as f64) <= view.max_x) + 1).min(bins.len());
    if lo >= hi {
        return TracePlan { spans };
    }

    let epoch_x = cache.epoch_x;
    let scale_x = 1.0 / NS_PER_SEC;
    upload_x.clear();
    upload_y.clear();
    // Point ranges of contiguous finite runs; each becomes one DrawSpan.
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut run_start = 0u32;
    for bin in &bins[lo..hi] {
        if !(bin.min.is_finite() && bin.max.is_finite()) {
            let count = upload_x.len() as u32;
            if count - run_start >= 2 {
                runs.push((run_start, count - run_start));
            }
            run_start = count;
            continue;
        }
        let x = ((bin.ts.0 as f64 - epoch_x) * scale_x) as f32;
        upload_x.push(x);
        upload_x.push(x);
        upload_y.push(((bin.min as f64 - y_epoch) * y_scale) as f32);
        upload_y.push(((bin.max as f64 - y_epoch) * y_scale) as f32);
    }
    let count = upload_x.len() as u32;
    if count - run_start >= 2 {
        runs.push((run_start, count - run_start));
    }

    let available = VALUE_CAPACITY - cache.cursor;
    let total = (upload_x.len() as u32).min(available);
    if total < upload_x.len() as u32 {
        cache.truncated = true;
        upload_x.truncate(total as usize);
        upload_y.truncate(total as usize);
    }
    if total == 0 {
        return TracePlan { spans };
    }

    let byte_offset = cache.cursor as u64 * 4;
    ctx.queue
        .write_buffer(x_buf, byte_offset, bytemuck::cast_slice(upload_x));
    ctx.queue
        .write_buffer(y_buf, byte_offset, bytemuck::cast_slice(upload_y));
    let chunk_offset = cache.cursor;
    cache.cursor += total;

    for (first, len) in runs {
        if first >= total {
            break;
        }
        let len = len.min(total - first);
        let index_available = (INDEX_CAPACITY as usize).saturating_sub(idx_scratch.len()) as u32;
        let len = if len > index_available {
            cache.truncated = true;
            index_available
        } else {
            len
        };
        if len < 2 {
            continue;
        }
        let span_first = idx_scratch.len() as u32;
        for j in 0..len {
            idx_scratch.push(chunk_offset + first + j);
        }
        spans.push(DrawSpan {
            instance_start: span_first,
            instance_end: span_first + len - 1,
        });
    }

    TracePlan { spans }
}

/// Upload one node-pair's decimated samples into the X/Y storage buffers.
///
/// Time-series traces with a real stride decimate by min-max selection —
/// each bin contributes its extreme samples so peaks survive — while
/// other sources point-sample. Uploads clamp to the shared buffer
/// capacity instead of vanishing; a clamp flags `cache.truncated`.
#[allow(clippy::too_many_arguments)]
fn upload_pair(
    ctx: &GpuContext,
    cache: &mut ValueCache,
    scratch_x: &mut Vec<f32>,
    scratch_y: &mut Vec<f32>,
    select_scratch: &mut Vec<u32>,
    x_buf: &wgpu::Buffer,
    y_buf: &wgpu::Buffer,
    x_node: &NodeView,
    y_node: &NodeView,
    x_source: &AxisSource<'_>,
    y_source: &AxisSource<'_>,
    y_epoch: f64,
    y_scale: f64,
    lod_stride: usize,
    lod_start: usize,
    lod_end: usize,
) -> Option<(u32, u32)> {
    if lod_end <= lod_start {
        return None;
    }
    let available = (VALUE_CAPACITY - cache.cursor) as usize;
    let epoch_x = cache.epoch_x;

    let minmax_y = match (x_source, y_source) {
        (AxisSource::Timestamps, AxisSource::Element { component, element_index, .. })
            if lod_stride > 1 =>
        {
            Some((&component.schema, *element_index))
        }
        _ => None,
    };

    let count = if let Some((schema, element_index)) = minmax_y {
        let data = y_node.full_data();
        select_minmax_indices(
            schema,
            data,
            element_index,
            lod_stride,
            lod_start,
            lod_end,
            select_scratch,
        );
        if select_scratch.len() > available {
            cache.truncated = true;
            select_scratch.truncate(available);
        }
        if select_scratch.is_empty() {
            return None;
        }
        let timestamps = y_node.full_timestamps();
        scratch_x.clear();
        scratch_x.reserve(select_scratch.len());
        for &src in select_scratch.iter() {
            let src = (src as usize).min(timestamps.len().saturating_sub(1));
            let raw = timestamps.get(src).map(|t| t.0 as f64).unwrap_or(0.0);
            scratch_x.push(((raw - epoch_x) * x_source.scale()) as f32);
        }
        scratch_y.clear();
        scratch_y.reserve(select_scratch.len());
        for &src in select_scratch.iter() {
            let v = read_element_f64(schema, data, src as usize, element_index).unwrap_or(0.0);
            scratch_y.push(((v - y_epoch) * y_scale) as f32);
        }
        select_scratch.len() as u32
    } else {
        let want = lod_end - lod_start;
        let take = want.min(available);
        if take < want {
            cache.truncated = true;
        }
        if take == 0 {
            return None;
        }
        let lod_end = lod_start + take;
        materialize_axis(
            x_source,
            x_node,
            lod_stride,
            lod_start,
            lod_end,
            epoch_x,
            x_source.scale(),
            scratch_x,
        );
        // Y is shifted by the axis minimum and scaled by 1/(max-min) so it
        // lands in [0,1]; the view uniform then maps that to clip space.
        materialize_axis(
            y_source, y_node, lod_stride, lod_start, lod_end, y_epoch, y_scale, scratch_y,
        );
        take as u32
    };

    let byte_offset = cache.cursor as u64 * 4;
    ctx.queue
        .write_buffer(x_buf, byte_offset, bytemuck::cast_slice(scratch_x));
    ctx.queue
        .write_buffer(y_buf, byte_offset, bytemuck::cast_slice(scratch_y));

    let offset = cache.cursor;
    cache.cursor += count;
    Some((offset, count))
}

/// Samples examined per decimation bin when hunting extremes. Bounds the
/// per-frame scan cost: a full sweep of every visible sample would make
/// zoomed-out frames O(data), while probing misses only features narrower
/// than `stride / MINMAX_BIN_PROBES` samples.
const MINMAX_BIN_PROBES: usize = 32;

/// Pick each bin's extreme samples: for bin `di` covering source samples
/// `[di*stride, (di+1)*stride)`, probe up to [`MINMAX_BIN_PROBES`] evenly
/// spaced candidates and emit the argmin and argmax in source order. The
/// result is an index list (≤ 2 per bin, deduplicated) that the caller
/// materializes for both axes, keeping `(x, y)` pairs honest.
fn select_minmax_indices(
    schema: &ComponentSchema,
    data: &[u8],
    element_index: usize,
    stride: usize,
    lod_start: usize,
    lod_end: usize,
    out: &mut Vec<u32>,
) {
    out.clear();
    let elem_size = schema.size();
    let max_sample = if elem_size == 0 {
        0
    } else {
        data.len() / elem_size
    };
    if max_sample == 0 {
        return;
    }
    out.reserve((lod_end - lod_start) * 2);
    let probe_step = (stride / MINMAX_BIN_PROBES).max(1);
    for di in lod_start..lod_end {
        let bin_start = (di * stride).min(max_sample - 1);
        let bin_end = ((di + 1) * stride).min(max_sample);
        let mut lo: (f64, usize) = (f64::INFINITY, bin_start);
        let mut hi: (f64, usize) = (f64::NEG_INFINITY, bin_start);
        let mut src = bin_start;
        while src < bin_end {
            let v = read_element_f64(schema, data, src, element_index).unwrap_or(0.0);
            if v < lo.0 {
                lo = (v, src);
            }
            if v > hi.0 {
                hi = (v, src);
            }
            src += probe_step;
        }
        let (first, second) = if lo.1 <= hi.1 { (lo.1, hi.1) } else { (hi.1, lo.1) };
        out.push(first as u32);
        if second != first {
            out.push(second as u32);
        }
    }
}

/// Materialize one axis's strided samples as `f32` in shifted plot space.
///
/// The output value is `(raw - epoch) * source.scale()`, written into
/// `out`. Raw timestamp axes pick up a 1e-9 scaling so f32 storage
/// retains nanosecond precision over long durations.
#[allow(clippy::too_many_arguments)]
fn materialize_axis(
    source: &AxisSource<'_>,
    node: &NodeView,
    lod_stride: usize,
    lod_start: usize,
    lod_end: usize,
    epoch: f64,
    scale: f64,
    out: &mut Vec<f32>,
) {
    out.clear();
    out.reserve(lod_end - lod_start);
    match source {
        AxisSource::Timestamps => {
            let ts = node.full_timestamps();
            if ts.is_empty() {
                for _ in lod_start..lod_end {
                    out.push(0.0);
                }
                return;
            }
            for di in lod_start..lod_end {
                let src = (di * lod_stride).min(ts.len() - 1);
                let v = (ts[src].0 as f64 - epoch) * scale;
                out.push(v as f32);
            }
        }
        AxisSource::Element {
            component,
            element_index,
            ..
        } => {
            let schema = &component.schema;
            convert_element_strided(
                schema,
                node.full_data(),
                *element_index,
                lod_stride,
                lod_start,
                lod_end,
                epoch,
                scale,
                out,
            );
        }
        // List-plot and envelope axes are handled by their own plan
        // paths and never routed through `materialize_axis`; fill with
        // zeros if somehow reached so the GPU buffer stays well-defined.
        AxisSource::LatestSampleIndex { .. }
        | AxisSource::LatestSampleElements { .. }
        | AxisSource::EnvelopeBins { .. } => {
            for _ in lod_start..lod_end {
                out.push(0.0);
            }
        }
    }
}

/// Pick the decimation stride for one node.
///
/// Shares the pixel budget proportionally across nodes by length; returns
/// `None` when this node deserves zero samples (skip it entirely).
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

/// Primitive types read directly from a mapped byte buffer during
/// decimation. `to_f64` centralises widening.
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

/// Decode one element at `sample_index` directly from a raw buffer.
///
/// `bool` doesn't implement `FromBytes` because not every byte pattern is
/// valid; callers handle `PrimType::Bool` by routing through `u8`.
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

/// Read one element as f64, dispatching on the schema's prim type.
fn read_element_f64(
    schema: &ComponentSchema,
    data: &[u8],
    sample_index: usize,
    element_index: usize,
) -> Option<f64> {
    let elem_size = schema.size();
    if elem_size == 0 {
        return None;
    }
    match schema.prim_type {
        PrimType::F64 => read_value::<f64>(data, sample_index, elem_size, element_index),
        PrimType::F32 => read_value::<f32>(data, sample_index, elem_size, element_index),
        PrimType::I64 => read_value::<i64>(data, sample_index, elem_size, element_index),
        PrimType::I32 => read_value::<i32>(data, sample_index, elem_size, element_index),
        PrimType::I16 => read_value::<i16>(data, sample_index, elem_size, element_index),
        PrimType::I8 => read_value::<i8>(data, sample_index, elem_size, element_index),
        PrimType::U64 => read_value::<u64>(data, sample_index, elem_size, element_index),
        PrimType::U32 => read_value::<u32>(data, sample_index, elem_size, element_index),
        PrimType::U16 => read_value::<u16>(data, sample_index, elem_size, element_index),
        PrimType::U8 | PrimType::Bool => {
            read_value::<u8>(data, sample_index, elem_size, element_index)
        }
    }
}

/// Materialize every `lod_stride`-th sample of one element into `f32`,
/// applying `(raw - epoch) * scale` per sample.
#[allow(clippy::too_many_arguments)]
fn convert_element_strided(
    schema: &ComponentSchema,
    full_data: &[u8],
    element_index: usize,
    lod_stride: usize,
    from: usize,
    to: usize,
    epoch: f64,
    scale: f64,
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

    #[allow(clippy::too_many_arguments)]
    fn fill<T: PlotValue>(
        data: &[u8],
        lod_stride: usize,
        from: usize,
        to: usize,
        max_sample: usize,
        elem_size: usize,
        elem_index: usize,
        epoch: f64,
        scale: f64,
        out: &mut Vec<f32>,
    ) {
        for decimated_i in from..to {
            let src = (decimated_i * lod_stride).min(max_sample.saturating_sub(1));
            let v = read_value::<T>(data, src, elem_size, elem_index).unwrap_or(0.0);
            out.push(((v - epoch) * scale) as f32);
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
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
            epoch,
            scale,
            out,
        ),
    }
}

/// Plan one list-plot trace from the component's latest sample.
///
/// Reads `component.time_series.latest()` once, decimates the resulting
/// vector to `pixel_budget`, uploads `(index, value)` pairs and emits a
/// single draw span. Skipped silently when the component has no samples
/// yet — the legend/auto-fit path will simply not see this trace.
#[allow(clippy::too_many_arguments)]
fn plan_list_trace(
    ctx: &GpuContext,
    cache: &mut ValueCache,
    upload_x: &mut Vec<f32>,
    upload_y: &mut Vec<f32>,
    idx_scratch: &mut Vec<u32>,
    x_buf: &wgpu::Buffer,
    y_buf: &wgpu::Buffer,
    component: &Component,
    len: usize,
    style: PlotStyle,
    y_epoch: f64,
    y_scale: f64,
    pixel_budget: usize,
) -> TracePlan {
    let mut spans = Vec::new();
    if len == 0 || pixel_budget == 0 {
        return TracePlan { spans };
    }
    let Some(latest) = component.time_series.latest() else {
        return TracePlan { spans };
    };
    let sample_bytes = latest.data();
    let schema = &component.schema;
    let prim_size = schema.prim_type.size();
    if prim_size == 0 || sample_bytes.len() < len * prim_size {
        return TracePlan { spans };
    }

    let stride = node_stride(len, len, pixel_budget).unwrap_or(1);
    let upload_stride = quantize_stride(stride);
    let want = len.div_ceil(upload_stride);
    let available = (VALUE_CAPACITY - cache.cursor) as usize;
    let count = want.min(available);
    if count < want {
        cache.truncated = true;
    }
    let count_u32 = count as u32;
    if count_u32 == 0 {
        return TracePlan { spans };
    }

    // X is the integer index sequence in raw plot space; shift by the
    // frame epoch like any other axis so it matches the view uniform.
    let epoch_x = cache.epoch_x;
    upload_x.clear();
    upload_x.reserve(count);
    for i in 0..count {
        upload_x.push(((i * upload_stride) as f64 - epoch_x) as f32);
    }

    upload_y.clear();
    upload_y.reserve(count);
    convert_latest_sample_strided(
        schema.prim_type,
        sample_bytes,
        len,
        upload_stride,
        prim_size,
        y_epoch,
        y_scale,
        upload_y,
    );

    let byte_offset = cache.cursor as u64 * 4;
    ctx.queue
        .write_buffer(x_buf, byte_offset, bytemuck::cast_slice(upload_x));
    ctx.queue
        .write_buffer(y_buf, byte_offset, bytemuck::cast_slice(upload_y));
    let chunk_offset = cache.cursor;
    cache.cursor += count_u32;

    let is_line = matches!(style, PlotStyle::Line);
    let span_first = idx_scratch.len() as u32;
    let index_available = (INDEX_CAPACITY as usize).saturating_sub(idx_scratch.len()) as u32;
    let count_u32 = if count_u32 > index_available {
        cache.truncated = true;
        index_available
    } else {
        count_u32
    };
    for j in 0..count_u32 {
        idx_scratch.push(chunk_offset + j);
    }
    let min_for_draw = if is_line { 2u32 } else { 1 };
    if count_u32 >= min_for_draw {
        let instance_end = if is_line {
            span_first + count_u32 - 1
        } else {
            span_first + count_u32
        };
        spans.push(DrawSpan {
            instance_start: span_first,
            instance_end,
        });
    } else {
        idx_scratch.truncate(span_first as usize);
    }

    TracePlan { spans }
}

/// Read every `lod_stride`-th element from a single sample's byte buffer
/// into `out` as `f32`. Sibling to [`convert_element_strided`], but the
/// stride moves through *element* offsets within one sample rather than
/// across samples.
#[allow(clippy::too_many_arguments)]
fn convert_latest_sample_strided(
    prim: PrimType,
    sample_bytes: &[u8],
    len: usize,
    lod_stride: usize,
    prim_size: usize,
    epoch: f64,
    scale: f64,
    out: &mut Vec<f32>,
) {
    #[allow(clippy::too_many_arguments)]
    fn fill<T: PlotValue>(
        data: &[u8],
        len: usize,
        lod_stride: usize,
        prim_size: usize,
        epoch: f64,
        scale: f64,
        out: &mut Vec<f32>,
    ) {
        let stride = lod_stride.max(1);
        let mut i = 0;
        while i < len {
            let offset = i * prim_size;
            let v = data
                .get(offset..offset + size_of::<T>())
                .and_then(|b| T::read_from_bytes(b).ok())
                .map(|v| v.to_f64())
                .unwrap_or(0.0);
            out.push(((v - epoch) * scale) as f32);
            i += stride;
        }
    }
    match prim {
        PrimType::F64 => fill::<f64>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::F32 => fill::<f32>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::I64 => fill::<i64>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::I32 => fill::<i32>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::I16 => fill::<i16>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::I8 => fill::<i8>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::U64 => fill::<u64>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::U32 => fill::<u32>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::U16 => fill::<u16>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out),
        PrimType::U8 | PrimType::Bool => {
            fill::<u8>(sample_bytes, len, lod_stride, prim_size, epoch, scale, out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_db::disruptor::Disruptor;
    use metor_db::time_series::{TimeSeries, TimeSeriesNode};
    use std::path::Path;
    use stellarator::util::AtomicCell;

    fn f64_schema() -> ComponentSchema {
        ComponentSchema::new(PrimType::F64, &[1])
    }

    #[test]
    fn minmax_selection_keeps_impulses() {
        let schema = f64_schema();
        let n = 32 * 64;
        let mut data = Vec::with_capacity(n * 8);
        for i in 0..n {
            let v = if i % 512 == 100 {
                1000.0
            } else {
                (i as f64 * 0.1).sin()
            };
            data.extend_from_slice(&v.to_le_bytes());
        }
        // stride == MINMAX_BIN_PROBES probes every sample in each bin.
        let stride = MINMAX_BIN_PROBES;
        let mut out = Vec::new();
        select_minmax_indices(&schema, &data, 0, stride, 0, n / stride, &mut out);
        for impulse in [100u32, 612, 1124, 1636] {
            assert!(
                out.contains(&impulse),
                "impulse at {impulse} dropped by decimation"
            );
        }
        assert!(out.len() <= 2 * (n / stride), "selection exceeded budget");
        assert!(out.windows(2).all(|w| w[0] < w[1]), "indices not ascending");
    }

    /// Build a component from nodes written straight to disk — no DB, no
    /// runtime — mirroring a reopened long-running session.
    fn synth_component(
        dir: &Path,
        nodes: usize,
        samples_per_node: usize,
        start_ns: i64,
        step_ns: i64,
    ) -> Component {
        for n in 0..nodes {
            let node_start = start_ns + (n * samples_per_node) as i64 * step_ns;
            let node =
                TimeSeriesNode::create(dir.join(node_start.to_string()), Timestamp(node_start), 8)
                    .unwrap();
            for i in 0..samples_per_node {
                let ts = node_start + i as i64 * step_ns;
                let v = (ts as f64 * 1e-9).sin();
                node.data.write(&v.to_le_bytes()).unwrap();
                node.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
            }
        }
        Component {
            component_id: ComponentId(1),
            time_series: TimeSeries::open(dir).unwrap(),
            wal: Disruptor::new(1024),
            schema: f64_schema(),
            last_timestamp: Arc::new(AtomicCell::new(Timestamp(0))),
        }
    }

    /// Long sessions at realistic nanosecond epochs must keep drawing at
    /// every zoom level: uploads stay inside the shared buffers and the
    /// epoch rebase keeps materialized x values small and ordered.
    #[test]
    fn long_range_renders_at_every_zoom() {
        let Some(mut gpu) = PlotGpu::try_new() else {
            eprintln!("skipping: no GPU available");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let start_ns: i64 = 1_700_000_000_000_000_000;
        let step_ns: i64 = 1_000_000; // 1 kHz
        let samples_per_node = 200_000;
        let nodes = 6;
        let component = synth_component(dir.path(), nodes, samples_per_node, start_ns, step_ns);
        let end_ns = start_ns + (nodes * samples_per_node) as i64 * step_ns;
        let target = RenderTarget::new(&gpu.ctx.device, 1500, 600);

        let windows = [
            (start_ns, end_ns),                           // full range (~20 min)
            (end_ns - 30_000_000_000, end_ns),            // last 30 s
            (end_ns - 1_000_000_000, end_ns),             // last 1 s (no decimation)
            (start_ns, start_ns + 60_000_000_000),        // first minute
        ];
        for (min_x, max_x) in windows {
            let draw = LineDraw {
                x: AxisSource::Timestamps,
                y: AxisSource::Element {
                    component_id: component.component_id,
                    component: &component,
                    element_index: 0,
                },
                y_min: -1.0,
                y_max: 1.0,
                style: PlotStyle::Line,
                color: Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 1.0,
                    a: 1.0,
                },
                stroke_width: 1.0,
                x_clip: None,
            };
            let view = PlotBounds::new(min_x as f64, 0.0, max_x as f64, 1.0);
            let drew = gpu.submit(&target, view, 1.0, std::slice::from_ref(&draw));
            let span_secs = (max_x - min_x) as f64 * 1e-9;
            assert!(drew, "no spans drawn for window of {span_secs}s");
            assert!(
                !gpu.cache.truncated,
                "decimated upload should fit capacity for window of {span_secs}s"
            );
            assert!(gpu.cache.cursor <= VALUE_CAPACITY);
            assert!(gpu.idx_scratch.len() <= INDEX_CAPACITY as usize);
            // Epoch rebase: the last uploaded chunk's x values are small
            // window-relative seconds, ordered, and finite.
            assert!(!gpu.upload_x.is_empty());
            assert!(
                gpu.upload_x
                    .iter()
                    .all(|x| x.is_finite() && *x >= -span_secs as f32 && *x <= 2.0 * span_secs as f32),
                "x values escaped the rebased window for {span_secs}s"
            );
            assert!(
                gpu.upload_x.windows(2).all(|w| w[0] <= w[1]),
                "x values out of order for {span_secs}s"
            );
        }
    }
}
