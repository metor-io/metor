//! Bevy world construction and the components systems in it rely on.
//!
//! Holds the plugin set, transform and render-layer propagation systems,
//! the readback observer, and the [`FrameSink`] component that pumps
//! rendered bytes back to the gpui thread. The actual [`App`] lives in
//! [`super::bridge::BevyBridge`] — everything here describes its shape.

use std::sync::Arc;

use bevy::window::{ExitCondition, WindowPlugin};
use bevy::{
    app::{App, SubApps},
    asset::{AssetPlugin, RenderAssetUsages, UnapprovedPathMode},
};
use bevy::{
    camera::visibility::RenderLayers,
    ecs::world::World,
    image::Image,
    prelude::*,
    render::{
        RenderPlugin,
        gpu_readback::ReadbackComplete,
        pipelined_rendering::PipelinedRenderingPlugin,
        render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
        renderer::{
            RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue,
            WgpuWrapper,
        },
        settings::{RenderCreation, RenderResources},
    },
};
use thingbuf::{ThingBuf, recycling::Recycle};

use crate::gpu_context::GpuContext;

/// Build the headless Bevy app and return its [`SubApps`].
///
/// `SubApps` is `Send`; the full `App` is not. Returning just the
/// sub-apps lets the bridge ship them between gpui and the render
/// worker without pinning to a thread. Mirrors the pattern in Bevy's
/// `externally_driven_headless_renderer` example.
pub(super) fn build_app() -> SubApps {
    let ctx = GpuContext::get().expect("metor-panel: no GPU adapter");

    let render_resources = RenderResources(
        RenderDevice::from(ctx.device.clone()),
        RenderQueue(Arc::new(WgpuWrapper::new(ctx.queue.clone()))),
        RenderAdapterInfo(WgpuWrapper::new(ctx.adapter.get_info())),
        RenderAdapter(Arc::new(WgpuWrapper::new(ctx.adapter.clone()))),
        RenderInstance(Arc::new(WgpuWrapper::new(ctx.instance.clone()))),
    );

    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(RenderPlugin {
                render_creation: RenderCreation::Manual(render_resources),
                synchronous_pipeline_compilation: true,
                ..default()
            })
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .set(AssetPlugin {
                unapproved_path_mode: UnapprovedPathMode::Allow,
                ..default()
            })
            // Pipelined rendering pins the render world to its own
            // thread, which breaks our model of migrating the app
            // between threads on each update.
            .disable::<PipelinedRenderingPlugin>(),
    );

    app.add_systems(
        Update,
        (compose_transforms, propagate_render_layers).chain(),
    );
    app.add_observer(on_readback_complete);

    // Externally-driven loop: finish + cleanup must run before the
    // first manual `update()`.
    app.finish();
    app.cleanup();

    std::mem::take(app.sub_apps_mut())
}

/// One rendered frame in the transit queue.
///
/// The `Vec<u8>` is recycled in place by [`ClearSlot`] so steady-state
/// rendering does zero heap work per frame.
#[derive(Default)]
pub(super) struct FrameSlot {
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub bytes: Vec<u8>,
}

/// Recycle policy: zero the header fields and clear the byte vec
/// without dropping its capacity.
pub(super) struct ClearSlot;

impl Recycle<FrameSlot> for ClearSlot {
    fn new_element(&self) -> FrameSlot {
        FrameSlot::default()
    }

    fn recycle(&self, element: &mut FrameSlot) {
        element.width = 0;
        element.height = 0;
        element.bytes.clear();
    }
}

/// Shared depth-2 queue between the readback observer and one viewer.
///
/// Most-recent-wins: if the viewer falls behind the observer discards
/// the oldest entry before pushing the new frame.
pub(super) type FrameQueue = Arc<ThingBuf<FrameSlot, ClearSlot>>;

pub(super) fn new_frame_queue() -> FrameQueue {
    Arc::new(ThingBuf::with_recycle(2, ClearSlot))
}

/// Attached to a viewer's readback sentinel entity.
///
/// Pairs the transit queue with the render-target size the observer
/// should stamp onto each outgoing frame. The size is updated whenever
/// the viewer swaps its render target.
#[derive(Component)]
pub(super) struct FrameSink {
    pub queue: FrameQueue,
    pub size: (u32, u32),
    pub generation: u64,
}

/// Marker requesting that a subtree inherit the root's [`RenderLayers`].
///
/// Bevy doesn't auto-propagate render layers through the hierarchy, so
/// child meshes in a GLTF scene would default to layer 0 and be
/// invisible to the per-viewer camera.
#[derive(Component)]
pub(super) struct PropagateRenderLayers;

/// Per-model transform inputs from the component streams.
///
/// A `None` axis is treated as identity, letting a stream drive only
/// translation or only rotation without touching the other.
#[derive(Component, Default, Clone, Copy)]
pub(super) struct LiveDelta {
    pub translation: Option<Vec3>,
    pub rotation: Option<Quat>,
    pub position_missing: bool,
    pub rotation_missing: bool,
}

/// System: apply each model's [`LiveDelta`] to its [`Transform`].
fn compose_transforms(mut q: Query<(&LiveDelta, &mut Transform, &mut Visibility)>) {
    for (delta, mut transform, mut visibility) in &mut q {
        *visibility = if delta.position_missing || delta.rotation_missing {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
        let mut out = Transform::IDENTITY;
        if let Some(t) = delta.translation {
            out.translation = t;
        }
        if let Some(r) = delta.rotation {
            out.rotation = r;
        }
        *transform = out;
    }
}

/// System: push [`RenderLayers`] into descendants that lack their own.
///
/// Runs every `Update`, so children spawned by asynchronous GLTF loading
/// pick up the viewer's layers on the frame after they appear.
fn propagate_render_layers(
    mut commands: Commands,
    roots: Query<(Entity, &RenderLayers), With<PropagateRenderLayers>>,
    children_q: Query<&Children>,
    existing: Query<(), With<RenderLayers>>,
) {
    for (root, layers) in &roots {
        propagate_layers_recursive(root, layers, &children_q, &existing, &mut commands);
    }
}

fn propagate_layers_recursive(
    entity: Entity,
    layers: &RenderLayers,
    children_q: &Query<&Children>,
    existing: &Query<(), With<RenderLayers>>,
    commands: &mut Commands,
) {
    let Ok(children) = children_q.get(entity) else {
        return;
    };
    for child in children.iter() {
        if existing.get(child).is_err() {
            commands.entity(child).insert(layers.clone());
        }
        propagate_layers_recursive(child, layers, children_q, existing, commands);
    }
}

/// Observer: when a readback completes on an entity carrying a
/// [`FrameSink`], push the bytes into the viewer's queue.
///
/// Drops the oldest queued frame if both slots are full so the latest
/// frame always wins.
fn on_readback_complete(trigger: On<ReadbackComplete>, mut sinks: Query<&mut FrameSink>) {
    let Ok(sink) = sinks.get_mut(trigger.entity) else {
        return;
    };
    let (w, h) = sink.size;
    let src = &trigger.data;

    loop {
        match sink.queue.push_ref() {
            Ok(mut slot) => {
                slot.generation = sink.generation;
                slot.width = w;
                slot.height = h;
                copy_tight_rows(src, &mut slot.bytes, w, h);
                return;
            }
            Err(_) => {
                // Full queue: drop the oldest and retry. Bail if the
                // pop fails so we don't spin on a genuinely empty queue.
                if sink.queue.pop_ref().is_none() {
                    return;
                }
            }
        }
    }
}

/// Strip wgpu's row padding from a readback.
///
/// When the row stride is already 256-aligned — the usual case after
/// `Viewer3d::quantize` rounds to a 64-pixel grid — this collapses to a
/// single memcpy. Writes into `dst`'s existing capacity so recycled
/// slots don't reallocate.
fn copy_tight_rows(src: &[u8], dst: &mut Vec<u8>, width: u32, height: u32) {
    let tight_row = (width as usize) * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
    let padded_row = tight_row.div_ceil(align) * align;
    let needed = tight_row * height as usize;
    dst.clear();
    dst.reserve(needed);
    if padded_row == tight_row {
        dst.extend_from_slice(&src[..needed.min(src.len())]);
        return;
    }
    for y in 0..height as usize {
        let start = y * padded_row;
        let end = start + tight_row;
        if end > src.len() {
            break;
        }
        dst.extend_from_slice(&src[start..end]);
    }
}

/// Allocate a BGRA render-target image.
///
/// `Bgra8UnormSrgb` matches the byte order `Window::paint_image` expects
/// and lets the PBR pipeline emit gamma-correct output without a manual
/// conversion step. Usage flags cover camera rendering, readback, and
/// debug texture binding.
pub(super) fn new_target_image(width: u32, height: u32) -> Image {
    let mut img = Image::new_uninit(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.texture_descriptor.usage |=
        TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC | TextureUsages::TEXTURE_BINDING;
    img
}

/// Batch-despawn several entities. `World::despawn` no-ops on missing
/// ids, so the viewer doesn't have to track which cells populated.
pub(super) fn despawn_entities(world: &mut World, entities: &[Entity]) {
    for entity in entities {
        world.despawn(*entity);
    }
}
