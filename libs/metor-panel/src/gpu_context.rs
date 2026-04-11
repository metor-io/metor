//! Process-wide wgpu device shared by all GPU-accelerated elements.
//!
//! Both the time-series line renderer and the 3D viewer's Bevy bridge hold an
//! `Arc<GpuContext>` so they render against the same adapter, device, and
//! queue. Bevy needs the `instance` and `adapter` in addition to the device
//! and queue to satisfy `RenderCreation::Manual`, which is why they're exposed
//! here even though the line renderer only uses `device` and `queue`.

use std::sync::{Arc, OnceLock};

/// Lazily-initialized wgpu resources shared across every renderer in the
/// process. Constructed on first access via [`GpuContext::get`].
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl GpuContext {
    /// Return the process-wide context, creating it on the first call. Returns
    /// `None` if no adapter is available (e.g. a headless CI host with no GPU).
    pub fn get() -> Option<Arc<GpuContext>> {
        static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
        CTX.get_or_init(|| match pollster::block_on(Self::create()) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(e) => {
                eprintln!("metor-panel: GpuContext init failed: {e}");
                None
            }
        })
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
        // Enable the full adapter feature set so Bevy's PBR pipelines (SSAO,
        // cluster assignment, mipmap downsampling) can create their bind
        // groups. Experimental features are excluded because requesting them
        // without `InstanceFlags::ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER` fails
        // device creation on most drivers.
        let experimental = wgpu::Features::EXPERIMENTAL_RAY_QUERY
            | wgpu::Features::EXPERIMENTAL_MESH_SHADER
            | wgpu::Features::EXPERIMENTAL_COOPERATIVE_MATRIX;
        let features = adapter.features() - experimental;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("metor-panel shared"),
                required_features: features,
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::Performance,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("wgpu request_device failed: {e:?}"))?;
        Ok(GpuContext {
            instance,
            adapter,
            device,
            queue,
        })
    }
}
