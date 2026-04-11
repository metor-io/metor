//! Bevy-side helpers for the 3D viewer.
//!
//! The Bevy [`App`] itself lives inside [`super::bridge::BevyBridge`] on the
//! GPUI thread. This module holds everything that goes *into* that App: the
//! plugin set, the components and systems a viewer element relies on, and
//! the single observer that ships readback bytes back to the GPUI side
//! through a per-viewer [`ThingBuf`] attached as a [`FrameSink`] component.
//!
//! Every mutation a viewer wants to make (spawn a camera, swap a render
//! target, apply a live transform) is expressed as an inline
//! `bridge.with_world(|w| …)` block at the call site — there are no
//! per-operation wrapper functions here. What lives in this file is the
//! *shared shape* every viewer agrees on: component types, the transform
//! composition system, the render-layer propagation system, and the
//! readback observer.

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

/// Build the headless Bevy app and immediately extract its [`SubApps`].
///
/// We return `SubApps` rather than the full `App` because `App` holds a
/// non-`Send` `RunnerFn` we don't use — every update is driven via
/// `sub_apps.update()` directly, the same pattern Bevy's
/// `examples/app/externally_driven_headless_renderer.rs` uses.
///
/// The returned `SubApps` is `Send`, so the bridge can ship it to a
/// background-executor worker for each render without thread pinning.
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
            // Pipelined rendering would put the render world on its own
            // dedicated thread, which fights the "ship the App between
            // threads on each render" model. Disable it so `app.update()`
            // runs the render graph synchronously on whichever thread is
            // currently driving the App.
            .disable::<PipelinedRenderingPlugin>(),
    );

    app.add_systems(
        Update,
        (compose_transforms, propagate_render_layers).chain(),
    );
    app.add_observer(on_readback_complete);

    // Externally-driven update loop: finish + cleanup must be called
    // before the first `update()`.
    app.finish();
    app.cleanup();

    std::mem::take(app.sub_apps_mut())
}

/// One rendered frame waiting to be consumed by the GPUI side. The
/// underlying `Vec<u8>` is reused across frames by the [`ClearSlot`]
/// recycler — the queue hands the writer a pre-allocated buffer that the
/// previous reader already emptied with `mem::take`, so steady-state
/// operation does zero allocations per frame.
#[derive(Default)]
pub(super) struct FrameSlot {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// [`thingbuf`] recycle policy for [`FrameSlot`]: new slots start empty,
/// recycled slots have their `Vec` cleared (keeping its capacity).
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

/// Shared readback queue between the Bevy world and one [`super::Viewer3d`].
/// Depth 2 with most-recent-wins semantics: if the viewer falls a frame
/// behind, the observer drops the oldest queued frame before pushing the
/// newest.
pub(super) type FrameQueue = Arc<ThingBuf<FrameSlot, ClearSlot>>;

/// Allocate a fresh depth-2 readback queue.
pub(super) fn new_frame_queue() -> FrameQueue {
    Arc::new(ThingBuf::with_recycle(2, ClearSlot))
}

/// Component attached to a viewer's readback sentinel entity. Holds the
/// shared [`ThingBuf`] the observer writes into plus the current
/// render-target dimensions (updated whenever the viewer swaps its render
/// target — the observer stamps these into every frame it produces).
#[derive(Component)]
pub(super) struct FrameSink {
    pub queue: FrameQueue,
    pub size: (u32, u32),
}

/// Marker for an entity whose children should inherit its [`RenderLayers`].
/// Bevy doesn't propagate `RenderLayers` through the hierarchy, so when a
/// GLTF scene is spawned under a tagged root the child mesh entities would
/// otherwise default to layer 0 and be invisible to every per-viewer camera.
#[derive(Component)]
pub(super) struct PropagateRenderLayers;

/// Live-binding delta from a component stream, written into the model's
/// [`Transform`] each frame by [`compose_transforms`]. Either field being
/// `None` leaves that axis at identity.
#[derive(Component, Default, Clone, Copy)]
pub(super) struct LiveDelta {
    pub translation: Option<Vec3>,
    pub rotation: Option<Quat>,
}

/// Write each model's live binding delta into its [`Transform`]. Either
/// axis being `None` is treated as identity for that axis.
fn compose_transforms(mut q: Query<(&LiveDelta, &mut Transform)>) {
    for (delta, mut transform) in &mut q {
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

/// Walk the descendants of every [`PropagateRenderLayers`] root and insert
/// the root's layers on any descendant that lacks its own. Runs every
/// frame in `Update`, which handles the asynchronous nature of GLTF scene
/// loading — newly-spawned child entities get their layers the frame after
/// they appear.
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
/// [`FrameSink`], copy the rendered bytes into the per-viewer queue.
///
/// Steady-state behavior: the queue has depth 2 so the viewer can miss a
/// single frame without forcing the observer to allocate. If both slots
/// are occupied (the viewer fell two frames behind), we discard the
/// oldest by draining one entry, then enqueue the newest — most-recent
/// frame wins.
fn on_readback_complete(trigger: On<ReadbackComplete>, mut sinks: Query<&mut FrameSink>) {
    let Ok(sink) = sinks.get_mut(trigger.entity) else {
        return;
    };
    let (w, h) = sink.size;
    let src = &trigger.data;

    loop {
        match sink.queue.push_ref() {
            Ok(mut slot) => {
                slot.width = w;
                slot.height = h;
                copy_tight_rows(src, &mut slot.bytes, w, h);
                return;
            }
            Err(_) => {
                // Queue is full: drop the oldest frame and retry.
                if sink.queue.pop_ref().is_none() {
                    // Nothing to pop and nothing to push — bail to avoid
                    // spinning. Shouldn't happen in practice because the
                    // observer is the only producer.
                    return;
                }
            }
        }
    }
}

/// Copy the padded readback rows into `dst` without any padding. wgpu
/// aligns `bytes_per_row` to `COPY_BYTES_PER_ROW_ALIGNMENT` (256); when
/// `width * 4` is already a multiple of that alignment (the common case
/// after `Viewer3d::quantize`'s 64-pixel rounding — 64 × 4 = 256) this is
/// a single flat memcpy. Writes into `dst`'s existing capacity whenever
/// possible so the recycled `Vec` doesn't reallocate.
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

/// Allocate a fresh BGRA render-target image at the requested size. The
/// usage flags cover the readback (`COPY_SRC`) + camera render pass
/// (`RENDER_ATTACHMENT`) + any debug texture binding. Bgra8UnormSrgb
/// matches the byte ordering GPUI's `Window::paint_image` expects and
/// lets Bevy's PBR pipeline produce gamma-correct output without a manual
/// transform.
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

/// Despawn each entity in `entities`. `World::despawn` is already a
/// non-panicking no-op for missing entities, so this is just a batching
/// convenience for call sites that have several handles to clean up at
/// once (e.g. a viewer tearing down its camera + light + readback).
pub(super) fn despawn_entities(world: &mut World, entities: &[Entity]) {
    for entity in entities {
        world.despawn(*entity);
    }
}
