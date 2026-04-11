//! Bevy-side helpers for the viewer bridge. The Bevy `App` itself is
//! owned by [`super::bridge::BevyBridge`] on the GPUI thread; this module
//! exposes:
//!
//! - [`build_app`] — constructs the headless App with our trimmed plugin
//!   set, [`RenderCreation::Manual`] using the shared [`GpuContext`], and
//!   the per-frame systems we need.
//! - Free functions that operate on a `&mut World` directly:
//!   [`create_viewer`], [`despawn_viewer`], [`resize_viewer`],
//!   [`set_viewer_camera`], [`load_model`], [`remove_model`],
//!   [`set_live_transform`].
//!
//! The free functions are called by the bridge either inline (when the
//! App is on the GPUI thread) or as queued closures drained after a
//! background render.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::app::{App, SubApps};
use bevy::asset::{AssetPlugin, AssetServer, RenderAssetUsages, UnapprovedPathMode};
use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::ecs::world::World;
use bevy::gltf::GltfAssetLabel;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::RenderPlugin;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::pipelined_rendering::PipelinedRenderingPlugin;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::renderer::{
    RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue, WgpuWrapper,
};
use bevy::render::settings::{RenderCreation, RenderResources};
use bevy::window::{ExitCondition, WindowPlugin};
use bevy::world_serialization::WorldAssetRoot;

use crate::gpu_context::GpuContext;

use super::bridge::{ModelId, ViewerId};

/// Build the headless Bevy app and immediately extract its [`SubApps`].
///
/// We return `SubApps` rather than the full `App` because `App` holds a
/// non-`Send` `RunnerFn` (`Box<dyn FnOnce(App) -> AppExit>`) that we
/// don't need — all updates are driven via `sub_apps.update()` directly,
/// the same pattern Bevy's `examples/app/externally_driven_headless_renderer.rs`
/// uses.
///
/// The returned `SubApps` is `Send` (its contents are: `World` is unsafely
/// `Send`, plugin registries hold `Box<dyn Plugin + Send + Sync>`), so the
/// bridge can ship it to a background-executor worker for each render
/// without thread pinning.
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

    app.init_resource::<LatestFrames>();
    app.add_systems(Update, (compose_transforms, propagate_render_layers).chain());
    app.add_observer(on_readback_complete);

    // Externally-driven update loop: finish + cleanup must be called
    // before the first `update()`.
    app.finish();
    app.cleanup();

    // Extract just the SubApps; the App's RunnerFn is dropped here.
    std::mem::take(app.sub_apps_mut())
}

/// Resource holding rendered frame bytes keyed by [`ViewerId`]. The
/// readback observer writes here every time a `ReadbackComplete` event
/// fires; the bridge drains this resource immediately after each
/// `app.update()` call on the background worker.
#[derive(Resource, Default)]
pub(super) struct LatestFrames(HashMap<ViewerId, Vec<u8>>);

/// Move every accumulated frame out of the world. Called by the bridge
/// after `app.update()` on the background worker.
pub(super) fn take_latest_frames(world: &mut World) -> HashMap<ViewerId, Vec<u8>> {
    std::mem::take(&mut world.resource_mut::<LatestFrames>().0)
}

/// Component tagging an entity (or readback sentinel) with the viewer it
/// belongs to. The readback observer uses this to route bytes back.
#[derive(Component, Clone, Copy)]
struct ViewerTag(ViewerId);

/// Component tagging a model entity with its per-viewer [`ModelId`]. Used
/// for despawn and `SetLiveTransform` lookups via tag queries.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
struct ModelTag(ModelId);

/// Marker for an entity whose children should inherit its [`RenderLayers`].
/// Bevy doesn't propagate `RenderLayers` through the hierarchy, so when a
/// GLTF scene is spawned under a tagged root the child mesh entities would
/// otherwise default to layer 0 and be invisible to every per-viewer camera.
#[derive(Component)]
struct PropagateRenderLayers;

/// Live-binding delta from a component stream, written into the model's
/// [`Transform`] each frame by [`compose_transforms`]. Either field being
/// `None` leaves that axis at identity.
#[derive(Component, Default, Clone, Copy)]
struct LiveDelta {
    translation: Option<Vec3>,
    rotation: Option<Quat>,
}

/// Map a [`ViewerId`] to its dedicated [`RenderLayers`] slot. Viewers
/// start at id 1, so layer 0 is intentionally unused (it's the default
/// layer for any untagged entity, which we don't want any viewer's camera
/// to see).
fn viewer_layer(id: ViewerId) -> RenderLayers {
    RenderLayers::from_layers(&[id.0 as usize])
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

/// Walk the descendants of every `PropagateRenderLayers` root and insert the
/// root's layers on any descendant that lacks its own. Runs every frame in
/// `Update`, which handles the asynchronous nature of GLTF scene loading —
/// newly-spawned child entities get their layers the frame after they appear.
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

/// Observer: when a readback completes on a tagged entity, store the
/// padded bytes in [`LatestFrames`] keyed by viewer id.
fn on_readback_complete(
    trigger: On<ReadbackComplete>,
    tags: Query<&ViewerTag>,
    mut latest: ResMut<LatestFrames>,
) {
    let entity = trigger.entity;
    let Ok(tag) = tags.get(entity) else {
        return;
    };
    latest.0.insert(tag.0, trigger.data.clone());
}

/// wgpu aligns `bytes_per_row` to `COPY_BYTES_PER_ROW_ALIGNMENT` (256).
/// Strip that padding so downstream consumers see a tight `width * 4`
/// byte stride.
pub(super) fn strip_row_padding(padded: &[u8], width: u32, height: u32) -> Vec<u8> {
    let tight_row = (width as usize) * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
    let padded_row = tight_row.div_ceil(align) * align;
    if padded_row == tight_row {
        return padded.to_vec();
    }
    let mut out = Vec::with_capacity(tight_row * height as usize);
    for y in 0..height as usize {
        let start = y * padded_row;
        let end = start + tight_row;
        if end > padded.len() {
            break;
        }
        out.extend_from_slice(&padded[start..end]);
    }
    out
}

// ---------------------------------------------------------------------
// Free functions invoked by the bridge to mutate the world directly.
// ---------------------------------------------------------------------

/// Spawn a viewer's camera, light, render-target image, and readback
/// sentinel. Called from `BevyBridge::register`.
pub(super) fn create_viewer(world: &mut World, id: ViewerId, width: u32, height: u32) {
    let target_image = new_target_image(width, height);
    let render_target = world.resource_mut::<Assets<Image>>().add(target_image);
    let layers = viewer_layer(id);

    world.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
        layers.clone(),
        ViewerTag(id),
    ));

    world.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(Color::srgb(0.05, 0.05, 0.08)),
            ..default()
        },
        RenderTarget::from(render_target.clone()),
        Projection::Perspective(PerspectiveProjection {
            fov: std::f32::consts::FRAC_PI_3,
            ..default()
        }),
        Transform::from_xyz(3.0, 2.0, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
        layers.clone(),
        ViewerTag(id),
    ));

    world.spawn((Readback::texture(render_target), ViewerTag(id)));
}

/// Despawn every entity tagged with `ViewerTag(id)`, plus any model
/// entities that belong to this viewer.
pub(super) fn despawn_viewer(world: &mut World, id: ViewerId) {
    let mut to_despawn: Vec<Entity> = Vec::new();
    let mut q = world.query::<(Entity, &ViewerTag)>();
    for (entity, tag) in q.iter(world) {
        if tag.0 == id {
            to_despawn.push(entity);
        }
    }
    for entity in to_despawn {
        world.despawn(entity);
    }
}

/// Replace a viewer's render-target image with a fresh one at the
/// requested size. Re-creates the readback sentinel so subsequent reads
/// target the new image.
pub(super) fn resize_viewer(world: &mut World, id: ViewerId, width: u32, height: u32) {
    let new_image = new_target_image(width, height);
    let new_handle = world.resource_mut::<Assets<Image>>().add(new_image);

    // Find the camera entity tagged with this viewer and re-attach the
    // new render target.
    let camera_entity = {
        let mut q = world.query::<(Entity, &ViewerTag, &Camera3d)>();
        q.iter(world)
            .find(|(_, tag, _)| tag.0 == id)
            .map(|(e, _, _)| e)
    };
    if let Some(camera) = camera_entity {
        world
            .entity_mut(camera)
            .insert(RenderTarget::from(new_handle.clone()));
    }

    // Despawn the old readback sentinel(s) and spawn a new one targeting
    // the new image.
    let old_readbacks: Vec<Entity> = {
        let mut q = world.query::<(Entity, &ViewerTag, &Readback)>();
        q.iter(world)
            .filter(|(_, tag, _)| tag.0 == id)
            .map(|(e, _, _)| e)
            .collect()
    };
    for entity in old_readbacks {
        world.despawn(entity);
    }
    world.spawn((Readback::texture(new_handle), ViewerTag(id)));
}

/// Move a viewer's camera to the given orbit pose.
pub(super) fn set_viewer_camera(
    world: &mut World,
    id: ViewerId,
    target: glam::Vec3,
    yaw: f32,
    pitch: f32,
    distance: f32,
    fov_y_rad: f32,
) {
    let camera_entity = {
        let mut q = world.query::<(Entity, &ViewerTag, &Camera3d)>();
        q.iter(world)
            .find(|(_, tag, _)| tag.0 == id)
            .map(|(e, _, _)| e)
    };
    let Some(entity) = camera_entity else {
        return;
    };
    let eye = orbit_eye(target, yaw, pitch, distance);
    let target_bevy = Vec3::new(target.x, target.y, target.z);
    let transform = Transform::from_translation(eye).looking_at(target_bevy, Vec3::Y);
    world
        .entity_mut(entity)
        .insert(transform)
        .insert(Projection::Perspective(PerspectiveProjection {
            fov: fov_y_rad,
            ..default()
        }));
}

fn orbit_eye(target: glam::Vec3, yaw: f32, pitch: f32, distance: f32) -> Vec3 {
    let t = Vec3::new(target.x, target.y, target.z);
    let cp = pitch.cos();
    let offset = Vec3::new(
        distance * cp * yaw.sin(),
        distance * pitch.sin(),
        distance * cp * yaw.cos(),
    );
    t + offset
}

/// Add or replace a model on a viewer.
pub(super) fn load_model(
    world: &mut World,
    viewer_id: ViewerId,
    model_id: ModelId,
    path: std::path::PathBuf,
) {
    // Despawn any existing model with the same id first.
    remove_model(world, viewer_id, model_id);

    let path_str = path.to_string_lossy().into_owned();
    let handle = world
        .resource::<AssetServer>()
        .load(GltfAssetLabel::Scene(0).from_asset(path_str));
    let layers = viewer_layer(viewer_id);
    world.spawn((
        WorldAssetRoot(handle),
        Transform::IDENTITY,
        LiveDelta::default(),
        layers,
        PropagateRenderLayers,
        ViewerTag(viewer_id),
        ModelTag(model_id),
    ));
}

/// Despawn one model from a viewer. No-op if the model isn't found.
pub(super) fn remove_model(world: &mut World, viewer_id: ViewerId, model_id: ModelId) {
    let entity = find_model(world, viewer_id, model_id);
    if let Some(entity) = entity {
        world.despawn(entity);
    }
}

/// Apply a live transform delta to one model.
pub(super) fn set_live_transform(
    world: &mut World,
    viewer_id: ViewerId,
    model_id: ModelId,
    translation: Option<glam::Vec3>,
    rotation: Option<glam::Quat>,
) {
    let Some(entity) = find_model(world, viewer_id, model_id) else {
        return;
    };
    let translation = translation.map(|v| Vec3::new(v.x, v.y, v.z));
    let rotation = rotation.map(|q| Quat::from_xyzw(q.x, q.y, q.z, q.w));
    world.entity_mut(entity).insert(LiveDelta {
        translation,
        rotation,
    });
}

fn find_model(world: &mut World, viewer_id: ViewerId, model_id: ModelId) -> Option<Entity> {
    let mut q = world.query::<(Entity, &ViewerTag, &ModelTag)>();
    q.iter(world)
        .find(|(_, vt, mt)| vt.0 == viewer_id && mt.0 == model_id)
        .map(|(e, _, _)| e)
}

fn new_target_image(width: u32, height: u32) -> Image {
    // Bgra8UnormSrgb matches the byte ordering GPUI's `Window::paint_image`
    // expects (BGRA premultiplied), and sRGB lets Bevy's PBR pipeline produce
    // gamma-correct output without a manual transform.
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
    img.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT
        | TextureUsages::COPY_SRC
        | TextureUsages::TEXTURE_BINDING;
    img
}
