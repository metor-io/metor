use bevy::prelude::*;
use bevy::window::{PrimaryWindow, WindowResized};
use metor_editor::EditorPlugin;
use miette::IntoDiagnostic;
use std::io::{Read, Seek, Write};
use stellarator::util::CancelToken;
use tokio::runtime::Runtime;

use super::Cli;

#[derive(Resource)]
struct WindowStateFile(std::fs::File);

impl Cli {
    pub fn editor(self, rt: Runtime) -> miette::Result<()> {
        let cancel_token = CancelToken::new();
        let mut app = self.editor_app()?;
        app.add_plugins(impeller2_bevy::TcpImpellerPlugin::new(self.addr));
        app.insert_resource(BevyCancelToken(cancel_token.clone()))
            .add_systems(Update, check_cancel_token);
        app.run();
        cancel_token.cancel();
        Ok(())
    }

    pub fn editor_app(&self) -> miette::Result<App> {
        let mut window_state_file = self.window_state_file()?;
        let mut window_state = String::new();
        window_state_file
            .read_to_string(&mut window_state)
            .into_diagnostic()?;
        let editor_plugin = if let [width, height] = window_state
            .split_whitespace()
            .collect::<Vec<_>>()
            .as_slice()
        {
            let width = width.parse::<f32>().into_diagnostic()?;
            let height = height.parse::<f32>().into_diagnostic()?;
            EditorPlugin::new(width, height)
        } else {
            EditorPlugin::default()
        };

        let mut app = App::new();
        app.insert_resource(WindowStateFile(window_state_file))
            .add_plugins(editor_plugin)
            .add_systems(Update, on_window_resize);
        Ok(app)
    }

    fn window_state_file(&self) -> miette::Result<std::fs::File> {
        use miette::Context;
        let dirs = self.dirs().into_diagnostic()?;
        let data_dir = dirs.data_dir();
        std::fs::create_dir_all(data_dir)
            .into_diagnostic()
            .context("failed to create data directory")?;
        let window_state_path = data_dir.join(".window-state");
        std::fs::File::options()
            .write(true)
            .read(true)
            .create(true)
            .truncate(false)
            .open(window_state_path)
            .into_diagnostic()
            .context("failed to open window state file")
    }
}

#[derive(Resource)]
struct BevyCancelToken(CancelToken);

fn check_cancel_token(token: Res<BevyCancelToken>, mut exit: EventWriter<AppExit>) {
    if token.0.is_cancelled() {
        exit.write(AppExit::Success);
    }
}

fn on_window_resize(
    mut window_state_file: ResMut<WindowStateFile>,
    mut resize_reader: EventReader<WindowResized>,
    query: Query<Entity, With<PrimaryWindow>>,
) {
    if let Some(e) = resize_reader.read().last() {
        if query.get(e.window).is_err() {
            return;
        }
        let window_state = format!("{:.1} {:.1}\n", e.width, e.height);
        if let Err(err) = window_state_file.0.rewind() {
            warn!(?err, "failed to rewind window state file");
            return;
        }
        if let Err(err) = window_state_file.0.write_all(window_state.as_bytes()) {
            warn!(?err, "failed to write window state");
        }
    }
}
