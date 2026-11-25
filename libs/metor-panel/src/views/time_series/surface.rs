//! IOSurface-backed plot frames (macOS).
//!
//! A plot resolves its MSAA render straight into a BGRA `CVPixelBuffer`
//! whose IOSurface is wrapped as a wgpu texture, and gpui composites that
//! buffer with `paint_surface`. No pixel leaves the GPU: this replaces the
//! texture → staging buffer → CPU → sprite atlas round trip.
//!
//! The buffer on screen is still being sampled by gpui while the next frame
//! renders, so each target keeps a small pool. A buffer is free again once
//! the pool holds its only reference: the plot's current frame, gpui's
//! scenes, and gpui's in-flight command buffers each hold one until they are
//! done with it.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};

use super::gpu::TARGET_FORMAT;

/// Most buffers one plot cycles through: one on screen, one rendering, and
/// headroom for frames gpui has queued on the GPU. A frame finding none free
/// is skipped, like one issued while a readback is in flight.
const POOL_SIZE: usize = 4;

/// A pixel buffer handed to gpui for one frame.
#[derive(Clone)]
pub(crate) struct SurfaceFrame(pub(crate) CFRetained<CVPixelBuffer>);

// SAFETY: CoreFoundation retain/release is thread-safe, and the buffer's
// pixels are only touched by the GPU, ordered by the submission wait.
unsafe impl Send for SurfaceFrame {}
unsafe impl Sync for SurfaceFrame {}

struct Slot {
    buffer: CFRetained<CVPixelBuffer>,
    view: wgpu::TextureView,
    _texture: wgpu::Texture,
}

pub(super) struct SurfacePool {
    width: u32,
    height: u32,
    slots: Vec<Slot>,
}

impl SurfacePool {
    /// `None` when the device isn't Metal or the first buffer can't be made,
    /// so the caller falls back to readback.
    pub(super) fn new(device: &wgpu::Device, width: u32, height: u32) -> Option<Self> {
        let first = Slot::new(device, width, height)?;
        Some(Self {
            width,
            height,
            slots: vec![first],
        })
    }

    /// A buffer nothing else holds, with the view to resolve into.
    pub(super) fn acquire(
        &mut self,
        device: &wgpu::Device,
    ) -> Option<(&wgpu::TextureView, SurfaceFrame)> {
        let free = match self
            .slots
            .iter()
            .position(|slot| slot.buffer.retain_count() == 1)
        {
            Some(i) => i,
            None if self.slots.len() < POOL_SIZE => {
                self.slots.push(Slot::new(device, self.width, self.height)?);
                self.slots.len() - 1
            }
            None => return None,
        };
        let slot = &self.slots[free];
        Some((&slot.view, SurfaceFrame(slot.buffer.clone())))
    }
}

impl Slot {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Option<Self> {
        let buffer = pixel_buffer(width, height)?;
        let iosurface = CVPixelBufferGetIOSurface(Some(&buffer))?;

        // SAFETY: the hal device outlives this call; the texture made from it
        // is wrapped back into wgpu on the same device below.
        let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
        let raw: Retained<ProtocolObject<dyn MTLTexture>> = unsafe {
            let desc =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    width as usize,
                    height as usize,
                    false,
                );
            desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            hal_device
                .raw_device()
                .newTextureWithDescriptor_iosurface_plane(&desc, &iosurface, 0)
        }?;
        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                raw,
                TARGET_FORMAT,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                None,
            )
        };
        drop(hal_device);

        // SAFETY: `hal_texture` was created on this device, matching `desc`.
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("plot surface"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: TARGET_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            )
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Some(Self {
            buffer,
            view,
            _texture: texture,
        })
    }
}

/// An IOSurface-backed, Metal-compatible BGRA pixel buffer.
fn pixel_buffer(width: u32, height: u32) -> Option<CFRetained<CVPixelBuffer>> {
    let empty = CFDictionary::<CFType, CFType>::empty();
    let keys = unsafe {
        [
            kCVPixelBufferIOSurfacePropertiesKey,
            kCVPixelBufferMetalCompatibilityKey,
        ]
    };
    let values: [&CFType; 2] = [&empty, CFBoolean::new(true)];
    let attributes = CFDictionary::from_slices(&keys, &values);
    let mut out = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            kCVPixelFormatType_32BGRA,
            Some(attributes.as_opaque()),
            std::ptr::NonNull::from(&mut out),
        )
    };
    if status != kCVReturnSuccess {
        tracing::warn!(status, width, height, "CVPixelBufferCreate failed");
        return None;
    }
    Some(unsafe { CFRetained::from_raw(std::ptr::NonNull::new(out)?) })
}

/// The buffer's pixels, rows unpadded, read on the CPU.
#[cfg(test)]
pub(super) fn read_pixels(frame: &SurfaceFrame) -> Vec<u8> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
        CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    let buffer = &frame.0;
    unsafe {
        assert_eq!(
            CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly),
            kCVReturnSuccess
        );
        let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let row = CVPixelBufferGetWidth(buffer) * 4;
        let mut out = Vec::with_capacity(row * CVPixelBufferGetHeight(buffer));
        for y in 0..CVPixelBufferGetHeight(buffer) {
            out.extend_from_slice(std::slice::from_raw_parts(base.add(y * stride), row));
        }
        CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
        out
    }
}
