//! Persisted 3D-viewer state.

use std::sync::Arc;

use gpui::{App, Context};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::{ModelEntry, OrbitCamera, Viewer3d};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Viewer3dPanelConfig {
    pub models: Vec<ModelConfig>,
    pub camera: CameraConfig,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelConfig {
    pub label: String,
    pub path: String,
    pub position_binding: Option<ComponentId>,
    pub orientation_binding: Option<ComponentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_expression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation_expression: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    pub target_x: f32,
    pub target_y: f32,
    pub target_z: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub fov_y_rad: f32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self::from(OrbitCamera::default())
    }
}

impl From<OrbitCamera> for CameraConfig {
    fn from(camera: OrbitCamera) -> Self {
        Self {
            target_x: camera.target.x,
            target_y: camera.target.y,
            target_z: camera.target.z,
            yaw: camera.yaw,
            pitch: camera.pitch,
            distance: camera.distance,
            fov_y_rad: camera.fov_y_rad,
        }
    }
}

impl From<CameraConfig> for OrbitCamera {
    fn from(config: CameraConfig) -> Self {
        Self {
            target: glam::Vec3::new(config.target_x, config.target_y, config.target_z),
            yaw: config.yaw,
            pitch: config.pitch,
            distance: config.distance,
            fov_y_rad: config.fov_y_rad,
        }
    }
}

impl From<&ModelEntry> for ModelConfig {
    fn from(model: &ModelEntry) -> Self {
        Self {
            label: model.label.to_string(),
            path: model.path.clone(),
            position_binding: model.position_binding_component(),
            orientation_binding: model.orientation_binding_component(),
            position_expression: model.position_binding.expression_text(),
            orientation_expression: model.orientation_binding.expression_text(),
        }
    }
}

impl Viewer3d {
    pub fn from_config(config: Viewer3dPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let mut viewer = Self::with_db(db.clone(), cx);
        for model in config.models {
            viewer.add_model(model.label, model.path, cx);
            if let Some(entry) = viewer.models.last().cloned() {
                entry.update(cx, |entry, cx| {
                    entry.position_binding = crate::data_binding::Binding::from_legacy(
                        model.position_binding.unwrap_or(ComponentId(0)),
                        model.position_expression.as_deref(),
                        &db,
                        cx,
                    );
                    entry.orientation_binding = crate::data_binding::Binding::from_legacy(
                        model.orientation_binding.unwrap_or(ComponentId(0)),
                        model.orientation_expression.as_deref(),
                        &db,
                        cx,
                    );
                    cx.notify();
                });
            }
        }
        viewer.camera = config.camera.into();
        viewer.camera_fov = viewer.camera.fov_y_rad;
        viewer.sync_camera(cx);
        viewer
    }

    pub fn to_config(&self, cx: &App) -> Viewer3dPanelConfig {
        Viewer3dPanelConfig {
            models: self
                .models
                .iter()
                .map(|model| ModelConfig::from(model.read(cx)))
                .collect(),
            camera: self.camera.into(),
        }
    }
}
