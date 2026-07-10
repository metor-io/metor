//! Shared wgpu resources reused by every GPU-backed view.
//!
//! The time-series renderer and the 3D viewer both target the same device and
//! queue so they can be composited by gpui without cross-device copies. Bevy's
//! `RenderCreation::Manual` additionally needs the `instance` and `adapter`,
//! which is why those are exposed here.
//!
//! Initialization is deliberately chatty at `info!`: the panel renders
//! off-screen and reads back to the CPU, so a GPU failure shows up as blank
//! views rather than a crash. The adapter/device logs are what a remote user
//! reports when that happens, and `WGPU_BACKEND=vulkan|dx12|gl` (honored via
//! [`wgpu::InstanceDescriptor::new_without_display_handle_from_env`]) is the
//! knob for steering them to a different backend.

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
                tracing::error!(
                    %e,
                    "GPU init failed — plots and the 3D viewer are disabled. Set RUST_LOG=info \
                     (and optionally WGPU_BACKEND=vulkan|dx12|gl) and report the 'wgpu adapter' \
                     log lines"
                );
                None
            }
        })
        .clone()
    }

    async fn create() -> Result<GpuContext, String> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());

        for adapter in instance.enumerate_adapters(wgpu::Backends::all()).await {
            let info = adapter.get_info();
            tracing::info!(
                name = %info.name,
                backend = %info.backend,
                device_type = ?info.device_type,
                driver = %info.driver_info,
                "wgpu adapter"
            );
        }

        let adapter = Self::pick_adapter(&instance).await?;
        let info = adapter.get_info();
        tracing::info!(
            name = %info.name,
            backend = %info.backend,
            device_type = ?info.device_type,
            "selected adapter"
        );

        // The plot pipelines render MSAA x4 into Bgra8Unorm and bind storage
        // buffers in the vertex stage. Both hold everywhere except weak GL
        // fallbacks; probe at init so a blank-plot report is diagnosable.
        let bgra = adapter.get_texture_format_features(wgpu::TextureFormat::Bgra8Unorm);
        if !bgra
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X4)
        {
            tracing::error!("adapter lacks 4x MSAA for Bgra8Unorm; plots will not render");
        }
        if !adapter
            .get_downlevel_capabilities()
            .flags
            .contains(wgpu::DownlevelFlags::VERTEX_STORAGE)
        {
            tracing::error!("adapter lacks vertex-stage storage buffers; plots will not render");
        }

        let (device, queue) = Self::request_device(&adapter).await?;
        Ok(GpuContext {
            instance,
            adapter,
            device,
            queue,
        })
    }

    /// Prefer a discrete GPU, then anything low-power, then the software
    /// rasterizer (WARP/llvmpipe) — slow but correct, and the readback-only
    /// pipeline never presents so it has no swapchain constraints.
    async fn pick_adapter(instance: &wgpu::Instance) -> Result<wgpu::Adapter, String> {
        let attempts = [
            (wgpu::PowerPreference::HighPerformance, false),
            (wgpu::PowerPreference::LowPower, false),
            (wgpu::PowerPreference::HighPerformance, true),
        ];
        let mut last_err = String::from("no adapter attempts made");
        for (power_preference, force_fallback_adapter) in attempts {
            match instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference,
                    compatible_surface: None,
                    force_fallback_adapter,
                })
                .await
            {
                Ok(adapter) => return Ok(adapter),
                Err(e) => {
                    tracing::warn!(
                        ?power_preference,
                        force_fallback_adapter,
                        %e,
                        "request_adapter failed"
                    );
                    last_err = format!("no wgpu adapter: {e:?}");
                }
            }
        }
        Err(last_err)
    }

    /// Ask for the adapter's full non-experimental feature set first (Bevy's
    /// PBR pipelines light up optional paths from `device.features()`), then
    /// fall back to the WebGPU baseline, which the plot pipelines and Bevy
    /// both run on.
    async fn request_device(
        adapter: &wgpu::Adapter,
    ) -> Result<(wgpu::Device, wgpu::Queue), String> {
        // Experimental features fail device creation without an unsafe opt-in;
        // subtract the whole mask rather than naming them so new ones can't
        // sneak back in. MAPPABLE_PRIMARY_BUFFERS tanks performance on
        // discrete GPUs (every buffer lands in host-visible memory).
        let mut features = adapter.features() - wgpu::Features::all_experimental_mask();
        if adapter.get_info().device_type == wgpu::DeviceType::DiscreteGpu {
            features = features - wgpu::Features::MAPPABLE_PRIMARY_BUFFERS;
        }
        let full = wgpu::DeviceDescriptor {
            label: Some("metor-panel shared"),
            required_features: features,
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::Performance,
            ..Default::default()
        };
        match adapter.request_device(&full).await {
            Ok(pair) => {
                tracing::info!("wgpu device created (full feature set)");
                return Ok(pair);
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    requested = ?features,
                    "full-feature request_device failed; retrying at WebGPU baseline"
                );
            }
        }
        let baseline = wgpu::DeviceDescriptor {
            label: Some("metor-panel shared"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::Performance,
            ..Default::default()
        };
        match adapter.request_device(&baseline).await {
            Ok(pair) => {
                tracing::info!("wgpu device created (WebGPU baseline)");
                Ok(pair)
            }
            Err(e) => Err(format!("wgpu request_device failed: {e:?}")),
        }
    }
}
