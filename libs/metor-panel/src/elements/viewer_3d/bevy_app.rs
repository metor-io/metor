//! Bevy-side half of the viewer bridge. Builds an `App` that reuses
//! metor-panel's shared [`GpuContext`], runs it on a dedicated thread via
//! `ScheduleRunnerPlugin`, and exposes two channels to the rest of the crate:
//! commands in and rendered frames out.
//!
//! The entry point is [`run`], which never returns — it owns the thread for
//! the lifetime of the process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::RenderPlugin;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::renderer::{
    RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue, WgpuWrapper,
};
use bevy::render::settings::{RenderCreation, RenderResources};
use bevy::window::{ExitCondition, WindowPlugin};
use crossbeam_channel::Receiver;

use crate::gpu_context::GpuContext;

use super::bridge::{FrameRouter, ViewerCommand, ViewerFrame, ViewerId};

/// Bevy thread entry point. Builds the App, wires the command/frame channels
/// into ECS resources, and enters `ScheduleRunnerPlugin`'s loop via `run()`.
pub(super) fn run(command_rx: Receiver<ViewerCommand>, frame_router: FrameRouter) {
    let Some(ctx) = GpuContext::get() else {
        // Nothing we can do without a GPU; the bridge thread just exits and
        // any Viewer3d elements will show empty frames.
        eprintln!("metor-panel: no GPU adapter available, 3D viewer disabled");
        return;
    };

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
                synchronous_pipeline_compilation: false,
                ..default()
            })
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            }),
    );
    app.add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
        1.0 / 60.0,
    )));

    app.insert_resource(CommandRx(command_rx));
    app.insert_resource(Router(frame_router));
    app.init_resource::<ViewerRegistry>();

    app.add_systems(Startup, setup_shared_scene);
    app.add_systems(Update, apply_commands);
    app.add_observer(on_readback_complete);

    app.run();
}

/// Holds the incoming command channel as a Bevy resource.
#[derive(Resource)]
struct CommandRx(Receiver<ViewerCommand>);

/// Holds the outgoing frame-router as a Bevy resource.
#[derive(Resource)]
struct Router(FrameRouter);

/// Tracks per-viewer entities so `Despawn` can clean them up.
#[derive(Resource, Default)]
struct ViewerRegistry {
    viewers: HashMap<ViewerId, ViewerRecord>,
}

struct ViewerRecord {
    camera: Entity,
    model: Entity,
    render_target: Handle<Image>,
    readback: Entity,
    width: u32,
    height: u32,
}

/// Component tagging an entity (or readback sentinel) with the viewer it
/// belongs to. The readback observer uses this to route bytes back.
#[derive(Component, Clone, Copy)]
struct ViewerTag(ViewerId);

/// One-time setup: directional light shared by every viewer. Per-viewer
/// RenderLayer isolation comes in a later phase; for now every viewer sees
/// the same light and (until models are added) the same test cube.
fn setup_shared_scene(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// Drain pending commands from GPUI and apply them to the world.
fn apply_commands(
    command_rx: Res<CommandRx>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut registry: ResMut<ViewerRegistry>,
    router: Res<Router>,
) {
    while let Ok(cmd) = command_rx.0.try_recv() {
        match cmd {
            ViewerCommand::Create { id, width, height } => {
                if registry.viewers.contains_key(&id) {
                    continue;
                }
                let record = spawn_viewer(
                    id,
                    width,
                    height,
                    &mut commands,
                    &mut images,
                    &mut meshes,
                    &mut materials,
                );
                registry.viewers.insert(id, record);
            }
            ViewerCommand::Despawn { id } => {
                if let Some(record) = registry.viewers.remove(&id) {
                    commands.entity(record.camera).despawn();
                    commands.entity(record.model).despawn();
                    commands.entity(record.readback).despawn();
                    images.remove(&record.render_target);
                    crate::elements::viewer_3d::bridge::BevyBridge::drop_router_entry(
                        &router.0, id,
                    );
                }
            }
            ViewerCommand::Resize { id, width, height } => {
                if let Some(record) = registry.viewers.get_mut(&id) {
                    if record.width == width && record.height == height {
                        continue;
                    }
                    // Build a new render target image; drop the old one. The
                    // readback component is re-created to target the new image.
                    let new_target = new_target_image(width, height);
                    let new_handle = images.add(new_target);
                    images.remove(&record.render_target);
                    record.render_target = new_handle.clone();
                    record.width = width;
                    record.height = height;

                    commands
                        .entity(record.camera)
                        .insert(RenderTarget::from(new_handle.clone()));

                    commands.entity(record.readback).despawn();
                    record.readback = commands
                        .spawn((Readback::texture(new_handle), ViewerTag(id)))
                        .id();
                }
            }
            ViewerCommand::SetCamera {
                id,
                target,
                yaw,
                pitch,
                distance,
                fov_y_rad,
            } => {
                if let Some(record) = registry.viewers.get(&id) {
                    let eye = orbit_eye(target, yaw, pitch, distance);
                    let target_bevy = Vec3::new(target.x, target.y, target.z);
                    let transform =
                        Transform::from_translation(eye).looking_at(target_bevy, Vec3::Y);
                    commands
                        .entity(record.camera)
                        .insert(transform)
                        .insert(Projection::Perspective(PerspectiveProjection {
                            fov: fov_y_rad,
                            ..default()
                        }));
                }
            }
        }
    }
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

fn spawn_viewer(
    id: ViewerId,
    width: u32,
    height: u32,
    commands: &mut Commands,
    images: &mut Assets<Image>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) -> ViewerRecord {
    let target_image = new_target_image(width, height);
    let render_target = images.add(target_image);

    // Test content: a single unlit-ish cube. Phase 8 replaces this with a
    // GLTF load.
    let model = commands
        .spawn((
            Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.55, 0.70, 0.95),
                perceptual_roughness: 0.4,
                ..default()
            })),
            Transform::from_xyz(0.0, 0.0, 0.0),
            ViewerTag(id),
        ))
        .id();

    let camera = commands
        .spawn((
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
            ViewerTag(id),
        ))
        .id();

    let readback = commands
        .spawn((Readback::texture(render_target.clone()), ViewerTag(id)))
        .id();

    ViewerRecord {
        camera,
        model,
        render_target,
        readback,
        width,
        height,
    }
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

/// Observer: when a readback completes on a tagged entity, strip wgpu row
/// padding and dispatch the frame bytes through the router.
fn on_readback_complete(
    trigger: On<ReadbackComplete>,
    tags: Query<&ViewerTag>,
    registry: Res<ViewerRegistry>,
    router: Res<Router>,
) {
    let entity = trigger.entity;
    let Ok(tag) = tags.get(entity) else {
        return;
    };
    let id = tag.0;
    let Some(record) = registry.viewers.get(&id) else {
        return;
    };

    let width = record.width;
    let height = record.height;
    let tight = strip_row_padding(&trigger.data, width, height);

    let router = router.0.lock().unwrap();
    if let Some(tx) = router.get(&id) {
        let _ = tx.try_send(ViewerFrame {
            id,
            width,
            height,
            rgba: tight,
        });
    }
}

/// wgpu aligns `bytes_per_row` to `COPY_BYTES_PER_ROW_ALIGNMENT` (256). Strip
/// that padding so downstream consumers see a tight `width * 4` byte stride.
fn strip_row_padding(padded: &[u8], width: u32, height: u32) -> Vec<u8> {
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
