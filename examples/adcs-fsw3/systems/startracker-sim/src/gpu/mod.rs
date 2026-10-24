//! The wgpu star-field renderer.
//!
//! The CPU [`Renderer`] prepares each frame: it selects the stars, projects
//! them in `f64` at every motion-blur sub-step and lists the truth. The GPU
//! takes those splats, a few thousand per frame, and does the per-pixel work:
//! the PSF splat into a fixed-point accumulator, then shot noise, read noise
//! and the ADC. Projecting on the CPU keeps the splat inputs bit-identical to
//! the CPU oracle's, which `f32`-only WGSL could not.
//!
//! Every buffer is made in [`GpuRenderer::new`]. The splat buffer grows if a
//! frame needs more room. Readback blocks until the frame is done.

mod layout;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use metor_star::render::noise::PoissonTable;
use metor_star::render::{Frame, RenderError, Renderer, Splat, Truth};
use wgpu::util::DeviceExt;
use zerocopy::{FromBytes, IntoBytes};

pub use layout::Params;

const SHADER: &str = include_str!("star_sim.wgsl");
const SPLAT_GROUP: u32 = 64;
const EXPOSE_GROUP: u32 = 256;
/// Splats the GPU holds room for before the first frame.
const SPLAT_CAPACITY: u64 = 1 << 15;
const WORD: u64 = 4;
/// The longest a frame's readback may block.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum GpuError {
    /// No adapter or device.
    NoDevice(String),
    /// The device reported an error.
    Device(String),
    /// A frame has more splats than one dispatch or binding holds.
    TooManySplats(u64),
    /// The image is not an even number of pixels.
    BadShape,
    Render(RenderError),
}

impl From<RenderError> for GpuError {
    fn from(e: RenderError) -> Self {
        GpuError::Render(e)
    }
}

/// The frame-sized GPU buffers.
struct Buffers {
    params: wgpu::Buffer,
    splats: wgpu::Buffer,
    acc: wgpu::Buffer,
    hits: wgpu::Buffer,
    hot: wgpu::Buffer,
    sky_cdf: wgpu::Buffer,
    image: wgpu::Buffer,
    staging: wgpu::Buffer,
    /// Splats `splats` holds.
    capacity: u64,
}

/// Renders frames of one [`Renderer`]'s scene on the GPU.
pub struct GpuRenderer<'a> {
    scene: Renderer<'a>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    splat: wgpu::ComputePipeline,
    expose: wgpu::ComputePipeline,
    buffers: Buffers,
    bind: wgpu::BindGroup,
    error: Arc<Mutex<Option<String>>>,
    /// Whether the last `map_async` mapped, once its callback has run.
    mapped: Arc<Mutex<Option<bool>>>,
}

impl<'a> GpuRenderer<'a> {
    /// Opens a headless device and uploads `scene`'s hot-pixel map.
    pub fn new(scene: Renderer<'a>) -> Result<Self, GpuError> {
        if scene.shape().pixels() % 2 != 0 {
            return Err(GpuError::BadShape);
        }
        let (device, queue) = pollster::block_on(open_device())?;
        let error = Arc::new(Mutex::new(None));
        device.on_uncaptured_error(Arc::new(record_error(error.clone())));
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("star_sim"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = bind_group_layout(&device);
        let pipelines = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("star_sim"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipelines),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let (splat, expose) = (pipeline("splat"), pipeline("expose"));
        let buffers = Buffers::new(&device, &scene, SPLAT_CAPACITY);
        let bind = buffers.bind(&device, &layout);
        let gpu = GpuRenderer {
            scene,
            device,
            queue,
            layout,
            splat,
            expose,
            buffers,
            bind,
            error,
            mapped: Arc::new(Mutex::new(None)),
        };
        gpu.check()?;
        Ok(gpu)
    }

    /// Renders `frame` into `out`, row-major, one DN per pixel, and returns
    /// what was drawn.
    pub fn render(&mut self, frame: &Frame, out: &mut [u16]) -> Result<Truth, GpuError> {
        if out.len() != self.scene.shape().pixels() {
            return Err(RenderError::BadOutput.into());
        }
        let mut truth = self.scene.prepare(frame);
        self.reserve(self.scene.splats().len() as u64)?;
        self.upload(frame);
        self.queue.submit([self.encode()]);
        truth.n_rendered = self.read_back(out)?;
        Ok(truth)
    }

    /// The last frame's accumulator, for tests. Allocates.
    pub fn read_acc(&self) -> Result<Vec<u32>, GpuError> {
        let size = self.buffers.acc.size();
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("acc readback"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(&self.buffers.acc, 0, &staging, 0, size);
        self.queue.submit([encoder.finish()]);
        let mut acc = vec![0u32; (size / WORD) as usize];
        self.map_into(&staging, acc.as_mut_bytes())?;
        Ok(acc)
    }

    /// The CPU side: cell selection, projection and truth.
    pub fn scene(&self) -> &Renderer<'a> {
        &self.scene
    }

    /// Grows the splat and hit buffers to hold `n` splats, within the
    /// device's dispatch and binding limits.
    fn reserve(&mut self, n: u64) -> Result<(), GpuError> {
        if n <= self.buffers.capacity {
            return Ok(());
        }
        let limits = self.device.limits();
        let dispatch =
            u64::from(limits.max_compute_workgroups_per_dimension) * u64::from(SPLAT_GROUP);
        let binding = limits.max_storage_buffer_binding_size / size_of::<Splat>() as u64;
        if n > dispatch.min(binding) {
            return Err(GpuError::TooManySplats(n));
        }
        let capacity = n.next_power_of_two();
        self.buffers.splats = splat_buffer(&self.device, capacity);
        self.buffers.hits = hit_buffer(&self.device, capacity);
        self.buffers.capacity = capacity;
        self.bind = self.buffers.bind(&self.device, &self.layout);
        Ok(())
    }

    fn upload(&self, frame: &Frame) {
        let splats = self.scene.splats();
        let exposure = self.scene.exposure(frame);
        let sky = PoissonTable::new(exposure.sky_e());
        let params = Params::new(&self.scene, &exposure, splats.len() as u32, sky.is_some());
        self.queue
            .write_buffer(&self.buffers.params, 0, params.as_bytes());
        if !splats.is_empty() {
            self.queue
                .write_buffer(&self.buffers.splats, 0, splats.as_bytes());
        }
        if let Some(sky) = sky {
            self.queue
                .write_buffer(&self.buffers.sky_cdf, 0, sky.cdf().as_bytes());
        }
    }

    fn encode(&self) -> wgpu::CommandBuffer {
        let b = &self.buffers;
        let n_splats = self.scene.splats().len() as u32;
        let pairs = (self.scene.shape().pixels() / 2) as u32;
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.clear_buffer(&b.acc, 0, None);
        encoder.clear_buffer(&b.hits, 0, Some((u64::from(n_splats) + 1) * WORD));
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_pipeline(&self.splat);
            pass.dispatch_workgroups(n_splats.div_ceil(SPLAT_GROUP), 1, 1);
            pass.set_pipeline(&self.expose);
            pass.dispatch_workgroups(pairs.div_ceil(EXPOSE_GROUP), 1, 1);
        }
        let image = b.image.size();
        encoder.copy_buffer_to_buffer(&b.image, 0, &b.staging, 0, image);
        encoder.copy_buffer_to_buffer(&b.hits, 0, &b.staging, image, WORD);
        encoder.finish()
    }

    /// Copies the frame's image into `out` and returns `n_rendered`.
    fn read_back(&self, out: &mut [u16]) -> Result<u32, GpuError> {
        let image = self.buffers.image.size() as usize;
        let mut words = [0u8; WORD as usize];
        self.map_with(&self.buffers.staging, |bytes| {
            out.as_mut_bytes().copy_from_slice(&bytes[..image]);
            words.copy_from_slice(&bytes[image..image + WORD as usize]);
        })?;
        Ok(u32::read_from_bytes(&words).unwrap_or(0))
    }

    fn map_into(&self, buffer: &wgpu::Buffer, out: &mut [u8]) -> Result<(), GpuError> {
        self.map_with(buffer, |bytes| out.copy_from_slice(&bytes[..out.len()]))
    }

    /// Maps `buffer` after the last submission, hands its bytes to `read`
    /// and unmaps it.
    fn map_with(&self, buffer: &wgpu::Buffer, read: impl FnOnce(&[u8])) -> Result<(), GpuError> {
        let done = self.mapped.clone();
        buffer.map_async(wgpu::MapMode::Read, .., move |r| {
            if let Ok(mut slot) = done.lock() {
                *slot = Some(r.is_ok());
            }
        });
        let wait = wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(FRAME_TIMEOUT),
        };
        let polled = self.device.poll(wait);
        let mapped = self.mapped.lock().ok().and_then(|mut slot| slot.take());
        let result = match (polled, mapped) {
            (Err(e), _) => Err(GpuError::Device(e.to_string())),
            (Ok(_), Some(true)) => buffer
                .get_mapped_range(..)
                .map(|view| read(&view))
                .map_err(|e| GpuError::Device(e.to_string())),
            (Ok(_), _) => Err(GpuError::Device("the readback did not map".into())),
        };
        buffer.unmap();
        result.and_then(|()| self.check())
    }

    /// The first error the device reported, if any. A device error is
    /// fatal: every later frame fails too.
    fn check(&self) -> Result<(), GpuError> {
        let error = self.error.lock().ok().and_then(|e| e.clone());
        error.map_or(Ok(()), |e| Err(GpuError::Device(e)))
    }
}

impl Buffers {
    fn new(device: &wgpu::Device, scene: &Renderer<'_>, capacity: u64) -> Buffers {
        let pixels = scene.shape().pixels() as u64;
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let buffer = |label, size, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let image_size = pixels * 2;
        let hot = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("hot"),
            contents: scene.hot().as_slice().as_bytes(),
            usage: wgpu::BufferUsages::STORAGE,
        });
        Buffers {
            params: buffer(
                "params",
                size_of::<Params>() as u64,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            ),
            splats: splat_buffer(device, capacity),
            acc: buffer("acc", pixels * WORD, storage | wgpu::BufferUsages::COPY_SRC),
            hits: hit_buffer(device, capacity),
            hot,
            sky_cdf: buffer("sky_cdf", size_of::<[f32; 168]>() as u64, storage),
            image: buffer("image", image_size, storage | wgpu::BufferUsages::COPY_SRC),
            staging: buffer(
                "staging",
                image_size + WORD,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            ),
            capacity,
        }
    }

    fn bind(&self, device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> wgpu::BindGroup {
        let buffers = [
            &self.params,
            &self.splats,
            &self.acc,
            &self.hits,
            &self.hot,
            &self.sky_cdf,
            &self.image,
        ];
        let entries: Vec<_> = (0..)
            .zip(buffers)
            .map(|(binding, b)| wgpu::BindGroupEntry {
                binding,
                resource: b.as_entire_binding(),
            })
            .collect();
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("star_sim"),
            layout,
            entries: &entries,
        })
    }
}

fn splat_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("splats"),
        size: capacity * size_of::<Splat>() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// The `n_rendered` word, then one flag per splat (at most one per star).
fn hit_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("hits"),
        size: (capacity + 1) * WORD,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

/// Binding 0 is the uniform params; 1, 4 and 5 are read-only storage; 2, 3
/// and 6 are read-write storage.
fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let storage = |read_only| wgpu::BufferBindingType::Storage { read_only };
    let entries = [
        entry(0, wgpu::BufferBindingType::Uniform),
        entry(1, storage(true)),
        entry(2, storage(false)),
        entry(3, storage(false)),
        entry(4, storage(true)),
        entry(5, storage(true)),
        entry(6, storage(false)),
    ];
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("star_sim"),
        entries: &entries,
    })
}

async fn open_device() -> Result<(wgpu::Device, wgpu::Queue), GpuError> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .map_err(|e| GpuError::NoDevice(e.to_string()))?;
    adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("star_sim"),
            ..Default::default()
        })
        .await
        .map_err(|e| GpuError::NoDevice(e.to_string()))
}

/// Keeps the first error the device reports.
fn record_error(slot: Arc<Mutex<Option<String>>>) -> impl Fn(wgpu::Error) + Send + Sync {
    move |e| {
        if let Ok(mut slot) = slot.lock() {
            slot.get_or_insert_with(|| e.to_string());
        }
    }
}

#[cfg(test)]
mod tests;
