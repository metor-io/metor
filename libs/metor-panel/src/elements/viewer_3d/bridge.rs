//! Bevy bridge: a single process-wide Bevy `App` that all `Viewer3d` elements
//! share. Each viewer registers with the bridge, gets a [`ViewerHandle`] back,
//! and communicates via crossbeam channels. Commands flow from GPUI into Bevy
//! (transforms, camera changes, model loads), and rendered frame bytes flow
//! back out, routed by [`ViewerId`] so each element only sees its own frames.
//!
//! The bridge is stored as a [`gpui::Global`] and lazy-spawned on the first
//! call to [`BevyBridge::get_or_init`]. The Bevy `App` runs on its own OS
//! thread via `ScheduleRunnerPlugin`, so the render graph never competes with
//! GPUI's paint thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use gpui::{App, Global};

use super::bevy_app;

/// Stable identifier for a live viewer. Minted by the bridge when a viewer
/// registers. The Bevy side tags entities with this id so cross-viewer queries
/// stay scoped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewerId(pub u64);

/// Stable identifier for a model within a single viewer. Minted by the
/// owning `Viewer3d` and never reused. The Bevy side keys
/// `ViewerRecord::models` by `ModelId` so commands can target one model
/// without depending on its position in any list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModelId(pub u64);

/// Commands flowing from GPUI into the Bevy world.
pub enum ViewerCommand {
    /// Register a new viewer with an initial render target size.
    Create { id: ViewerId, width: u32, height: u32 },
    /// Tear down a viewer and free its GPU resources.
    Despawn { id: ViewerId },
    /// Resize an existing viewer's render target.
    Resize { id: ViewerId, width: u32, height: u32 },
    /// Update a viewer's camera pose.
    SetCamera {
        id: ViewerId,
        target: glam::Vec3,
        yaw: f32,
        pitch: f32,
        distance: f32,
        fov_y_rad: f32,
    },
    /// Add or replace a single model in the viewer. Replaces any existing
    /// model with the same `model_id`. The path is resolved by Bevy's
    /// `AssetServer`, so absolute paths work anywhere.
    LoadModel {
        id: ViewerId,
        model_id: ModelId,
        path: std::path::PathBuf,
    },
    /// Despawn one model from the viewer. No-op if the model_id is unknown.
    RemoveModel { id: ViewerId, model_id: ModelId },
    /// Apply a live position and/or orientation delta to one specific
    /// model. Setting either component to `None` leaves that axis at
    /// identity; both `None` parks the model at its origin.
    SetLiveTransform {
        id: ViewerId,
        model_id: ModelId,
        translation: Option<glam::Vec3>,
        rotation: Option<glam::Quat>,
    },
}

/// Rendered frame sent back to a [`ViewerHandle`].
pub struct ViewerFrame {
    pub id: ViewerId,
    pub width: u32,
    pub height: u32,
    /// Tightly-packed `Rgba8UnormSrgb` bytes: `width * height * 4` bytes. Row
    /// padding from wgpu's `COPY_BYTES_PER_ROW_ALIGNMENT` has already been
    /// stripped on the Bevy side.
    pub rgba: Vec<u8>,
}

/// Per-viewer handle returned by [`BevyBridge::register`]. Dropping this
/// automatically sends a `Despawn` command, so the Viewer3d element just needs
/// to hold it and the Bevy side will clean up when the element goes away.
pub struct ViewerHandle {
    id: ViewerId,
    command_tx: Sender<ViewerCommand>,
    frame_rx: Receiver<ViewerFrame>,
}

impl ViewerHandle {
    pub fn id(&self) -> ViewerId {
        self.id
    }

    pub fn send(&self, cmd: ViewerCommand) {
        let _ = self.command_tx.send(cmd);
    }

    pub fn frame_rx(&self) -> &Receiver<ViewerFrame> {
        &self.frame_rx
    }
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        let _ = self.command_tx.send(ViewerCommand::Despawn { id: self.id });
    }
}

/// Shared routing table the Bevy side uses to dispatch rendered frames back to
/// the correct [`ViewerHandle`].
pub(crate) type FrameRouter = Arc<Mutex<HashMap<ViewerId, Sender<ViewerFrame>>>>;

/// Process-wide bridge to the Bevy app. Stored as a `gpui::Global`; spawn is
/// lazy on first use.
pub struct BevyBridge {
    command_tx: Sender<ViewerCommand>,
    frame_router: FrameRouter,
    next_id: AtomicU64,
    _thread: JoinHandle<()>,
}

impl Global for BevyBridge {}

impl BevyBridge {
    /// Fetch or create the process-wide bridge.
    pub fn get_or_init(cx: &mut App) -> &mut BevyBridge {
        if !cx.has_global::<BevyBridge>() {
            cx.set_global(BevyBridge::spawn());
        }
        cx.global_mut::<BevyBridge>()
    }

    fn spawn() -> Self {
        let (command_tx, command_rx) = unbounded::<ViewerCommand>();
        let frame_router: FrameRouter = Arc::new(Mutex::new(HashMap::new()));
        let router_for_thread = frame_router.clone();
        let thread = std::thread::Builder::new()
            .name("metor-bevy".into())
            .spawn(move || bevy_app::run(command_rx, router_for_thread))
            .expect("spawn metor-bevy thread");
        Self {
            command_tx,
            frame_router,
            next_id: AtomicU64::new(1),
            _thread: thread,
        }
    }

    /// Register a new viewer. Sends a `Create` command to Bevy and installs a
    /// per-viewer frame sender in the router.
    pub fn register(&self, width: u32, height: u32) -> ViewerHandle {
        let id = ViewerId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (frame_tx, frame_rx) = unbounded::<ViewerFrame>();
        self.frame_router.lock().unwrap().insert(id, frame_tx);
        let _ = self
            .command_tx
            .send(ViewerCommand::Create { id, width, height });
        ViewerHandle {
            id,
            command_tx: self.command_tx.clone(),
            frame_rx,
        }
    }

    /// Remove a viewer from the router. Called by the Bevy side after a
    /// `Despawn` is fully processed; exposed here for symmetry.
    pub(crate) fn drop_router_entry(router: &FrameRouter, id: ViewerId) {
        router.lock().unwrap().remove(&id);
    }
}
