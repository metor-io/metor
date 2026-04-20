//! Shared wgpu resources reused by every GPU-backed view.
//!
//! The time-series renderer and the 3D viewer both target the same device and
//! queue so they can be composited by gpui without cross-device copies. Bevy's
//! `RenderCreation::Manual` additionally needs the `instance` and `adapter`,
//! which is why those are exposed here.

use std::sync::{Arc, OnceLock};

/// Process-wide wgpu handles, created lazily on first [`GpuContext::get`].
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl GpuContext {
    /// Return the shared context. Returns `None` when no adapter is available
    /// (typical of headless CI hosts); views should fall back to a no-GPU path
    /// rather than panic.
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
        // Bevy's PBR pipelines need the full non-experimental feature set;
        // experimental features fail device creation without an unsafe flag.
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
