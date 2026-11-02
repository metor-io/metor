//! Resolve storage before creating a DB, server, or connection worker.
use std::path::PathBuf;

use gpui::{
    App, AppContext as _, Bounds, Context, Focusable as _, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions, point, px, size,
};

use super::SessionStorage;
use crate::PanelApp;

#[cfg(target_os = "macos")]
mod macos;

pub(crate) struct SessionStartup {
    app: Option<PanelApp>,
    selecting: bool,
    path: Option<PathBuf>,
    pub(crate) error: Option<SharedString>,
    opening: Option<Opening>,
    /// The peer being asked who it is before its session DB is chosen.
    probing: Option<Probing>,
}

struct Probing {
    addr: std::net::SocketAddr,
    /// Set by Escape: the probe's result, whenever it lands, is dropped.
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

type OpenResult = std::io::Result<(
    SessionStorage,
    std::sync::Arc<metor_db::DB>,
    super::archive::Imported,
)>;
struct Opening {
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<OpenResult>>,
}
impl Drop for Opening {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn open(mut app: PanelApp, path: Option<PathBuf>, cx: &mut App) {
    let store = app.prepare_connections(path.is_none(), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(860.), px(480.)),
            cx,
        ))),
        is_resizable: false,
        window_background: if cfg!(target_os = "macos") {
            gpui::WindowBackgroundAppearance::Transparent
        } else {
            gpui::WindowBackgroundAppearance::Opaque
        },
        titlebar: Some(TitlebarOptions {
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(8.))),
            ..Default::default()
        }),
        window_decorations: Some(gpui::WindowDecorations::Client),
        app_id: Some("metor-panel".into()),
        ..Default::default()
    };
    if let Err(error) = cx.open_window(options, |window, cx| {
        #[cfg(target_os = "macos")]
        if let Err(error) = macos::install_sidebar_blur(window, &crate::theme::theme(cx)) {
            tracing::warn!(%error, "could not install native splash blur");
            window.set_background_appearance(gpui::WindowBackgroundAppearance::Blurred);
        }
        window.on_window_should_close(cx, |_, cx| {
            cx.quit();
            true
        });
        let startup = cx.new(|_| SessionStartup {
            app: Some(app),
            selecting: false,
            path: None,
            error: None,
            opening: None,
            probing: None,
        });
        let picker = cx.new(|cx| {
            crate::connections::ConnectionPicker::for_startup(store, startup.clone(), cx)
        });
        picker.focus_handle(cx).focus(window, cx);
        let weak = startup.downgrade();
        cx.on_app_quit(move |cx| {
            let _ = weak.update(cx, |picker, _| {
                picker.opening.take();
            });
            async {}
        })
        .detach();
        if let Some(path) = path {
            startup.update(cx, |startup, cx| startup.open_path(path, window, cx));
        }
        picker
    }) {
        tracing::error!(%error, "could not open session picker");
        cx.quit();
    }
}

impl SessionStartup {
    pub(crate) fn connect(
        &mut self,
        target: crate::connections::ConnectionTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.app.is_none() || self.busy() {
            return;
        }
        let result = match (self.path.clone(), target.peer) {
            (Some(path), _) => SessionStorage::record_to(path),
            (None, Some(addr)) => {
                // Which DB depends on who answers, so ask first, off the UI
                // thread; the screen shows the wait as busy.
                let config = self.app.as_ref().unwrap().storage_config().clone();
                let cancel = std::sync::Arc::<std::sync::atomic::AtomicBool>::default();
                let selected = super::select_for_peer(addr, config, cancel.clone());
                self.probing = Some(Probing {
                    addr,
                    cancel: cancel.clone(),
                });
                self.error = None;
                cx.spawn_in(window, async move |this, cx| {
                    let selected = selected.await;
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        discard(selected);
                        return;
                    }
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.probing = None;
                        this.begin(selected, target, window, cx);
                    });
                })
                .detach();
                cx.notify();
                return;
            }
            (None, None) => SessionStorage::create(),
        };
        self.begin(result, target, window, cx);
    }

    /// Start the session on the storage `connect` chose, or show why there
    /// is none.
    fn begin(
        &mut self,
        result: super::peer::Selected,
        target: crate::connections::ConnectionTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((storage, db)) => {
                if storage.is_recording() {
                    let parent = storage
                        .directory
                        .path()
                        .parent()
                        .and_then(|p| p.canonicalize().ok())
                        .and_then(|p| p.to_str().map(str::to_owned));
                    if let Some(settings) = cx.try_global::<crate::theme::FontSettings>() {
                        let mut config = settings.config.clone();
                        config.recording_parent = parent;
                        if let Err(error) = crate::config::save(&config) {
                            tracing::warn!(%error, "could not remember recording folder");
                        }
                        cx.global_mut::<crate::theme::FontSettings>().config = config;
                    }
                }
                self.app
                    .take()
                    .unwrap()
                    .start_connected(storage, db, target, cx);
                window.remove_window();
            }
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
            }
        }
    }

    pub(crate) fn choose_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.selecting = true;
        let receiver = cx.prompt_for_paths(super::open_recording_prompt());
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.selecting = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.open_path(path, window, cx);
                        }
                    }
                    Ok(Err(error)) => this.error = Some(error.to_string().into()),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || self.app.is_none() {
            return;
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        self.error = None;
        self.opening = Some(Opening {
            cancel,
            worker: Some(std::thread::spawn(move || {
                SessionStorage::import(path, &worker_cancel)
            })),
        });
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let keep_polling = this
                    .update_in(cx, |this, window, cx| {
                        let Some(opening) = &mut this.opening else {
                            return false;
                        };
                        if !opening.worker.as_ref().unwrap().is_finished() {
                            return true;
                        }
                        let result = opening
                            .worker
                            .take()
                            .unwrap()
                            .join()
                            .map_err(|_| std::io::Error::other("Import worker panicked"))
                            .and_then(|r| r);
                        this.opening = None;
                        match result {
                            Ok((storage, db, imported)) => {
                                this.app.take().unwrap().start_imported(
                                    storage,
                                    db,
                                    imported.workspace,
                                    cx,
                                );
                                window.remove_window();
                            }
                            Err(error) => {
                                this.error = Some(error.to_string().into());
                                cx.notify();
                            }
                        }
                        false
                    })
                    .unwrap_or(false);
                if !keep_polling {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn record_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || self.app.is_none() {
            return;
        }
        self.selecting = true;
        self.error = None;
        let parent = cx
            .try_global::<crate::theme::FontSettings>()
            .and_then(|s| s.config.recording_parent.as_ref())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        let receiver = cx.prompt_for_new_path(&parent, Some("recording.metor"));
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, _window, cx| {
                this.selecting = false;
                match result {
                    Ok(Ok(Some(path))) => {
                        // Selection does not create a DB. Connect reserves the
                        // name exclusively before starting any producers.
                        match super::bundle_path(path).and_then(|path| {
                            match std::fs::symlink_metadata(&path) {
                                Ok(_) => Err(std::io::Error::other(
                                    "Choose a new recording path; this path already exists",
                                )),
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    Ok(path)
                                }
                                Err(error) => Err(error),
                            }
                        }) {
                            Ok(path) => this.path = Some(path),
                            Err(error) => this.error = Some(error.to_string().into()),
                        }
                    }
                    Ok(Err(error)) => this.error = Some(error.to_string().into()),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl SessionStartup {
    pub(crate) fn busy(&self) -> bool {
        self.selecting || self.opening.is_some() || self.probing.is_some()
    }

    pub(crate) fn temporary(&mut self, cx: &mut Context<Self>) {
        if !self.busy() {
            self.path = None;
            self.error = None;
            cx.notify();
        }
    }

    pub(crate) fn is_temporary(&self) -> bool {
        self.path.is_none()
    }

    pub(crate) fn write_location_label(&self) -> SharedString {
        self.path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned().into())
            .unwrap_or_else(|| "Temporary".into())
    }

    pub(crate) fn summary(&self) -> Option<SharedString> {
        if self.opening.is_some() {
            Some("Opening recording… Press Escape to cancel.".into())
        } else if let Some(probing) = &self.probing {
            Some(format!("Connecting to {}… Press Escape to cancel.", probing.addr).into())
        } else if self.selecting {
            None
        } else {
            self.path
                .as_ref()
                .map(|path| format!("Record to {}", path.display()).into())
        }
    }

    /// Escape while busy: stop opening a recording, or stop waiting for a
    /// peer to answer and return to the idle screen.
    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(opening) = &self.opening {
            opening
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(probing) = self.probing.take() {
            probing
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            cx.notify();
        }
    }
}

/// Release storage chosen for a connect that was cancelled meanwhile: a
/// temporary DB is deleted, a peer cache flushed and unlocked. Off the UI
/// thread, since that can mean stopping a DB's executor.
fn discard(selected: super::peer::Selected) {
    if let Ok((storage, db)) = selected {
        std::thread::spawn(move || {
            if let Err(error) = storage.finish(&db, Ok(())) {
                tracing::warn!(error = %error.error, "could not release a cancelled session's db");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn cancelling_or_rejecting_a_path_keeps_startup_uninitialized(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let picker = cx.new(|_| SessionStartup {
            app: Some(PanelApp::choose_session()),
            selecting: false,
            path: None,
            error: None,
            opening: None,
            probing: None,
        });
        cx.update(|window, cx| picker.update(cx, |picker, cx| picker.record_to(window, cx)));
        assert!(cx.did_prompt_for_new_path());
        cx.simulate_new_path_selection(|_| None);
        cx.run_until_parked();
        cx.update(|_, cx| {
            let picker = picker.read(cx);
            assert!(!picker.selecting);
            assert!(picker.app.is_some());
            assert!(picker.error.is_none());
            assert!(
                cx.try_global::<crate::background_tasks::BackgroundTasks>()
                    .is_none()
            );
            assert!(crate::connections::try_global(cx).is_none());
        });

        let target = tempfile::tempdir().unwrap();
        let path = target.path().join("existing.metor");
        std::fs::write(&path, b"keep").unwrap();
        cx.update(|window, cx| picker.update(cx, |picker, cx| picker.record_to(window, cx)));
        cx.simulate_new_path_selection(|_| Some(path.clone()));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let picker = picker.read(cx);
            assert!(!picker.selecting);
            assert!(picker.app.is_some());
            assert!(picker.error.is_some());
            assert!(crate::connections::try_global(cx).is_none());
        });
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }
    #[gpui::test]
    fn escape_while_probing_a_peer_returns_to_idle(cx: &mut gpui::TestAppContext) {
        // Accepts the connection and never answers, so the probe waits out
        // its timeout.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let target = crate::connections::ConnectionTarget::tcp("Silent", addr);
        let visual = cx.add_empty_window();
        let startup = visual.new(|_| SessionStartup {
            app: Some(PanelApp::choose_session()),
            selecting: false,
            path: None,
            error: None,
            opening: None,
            probing: None,
        });
        visual.update(|window, cx| {
            startup.update(cx, |startup, cx| startup.connect(target, window, cx))
        });
        visual.update(|_, cx| {
            let startup = startup.read(cx);
            assert!(startup.busy());
            assert!(startup.summary().unwrap().contains("Escape"));
        });
        visual.update(|_, cx| startup.update(cx, |startup, cx| startup.cancel(cx)));
        visual.update(|_, cx| assert!(!startup.read(cx).busy()));

        // The probe ends after cancelling; nothing starts from it.
        std::thread::sleep(
            super::super::peer::PROBE_TIMEOUT + std::time::Duration::from_millis(500),
        );
        visual.run_until_parked();
        visual.update(|_, cx| {
            let startup = startup.read(cx);
            assert!(startup.app.is_some() && !startup.busy());
            assert!(startup.error.is_none());
            assert!(crate::connections::try_global(cx).is_none());
        });
    }

    #[gpui::test]
    fn choosing_storage_does_not_start_a_session_and_temporary_resets_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let target = tempfile::tempdir().unwrap();
        let path = target.path().join("new.metor");
        let visual = cx.add_empty_window();
        let startup = visual.new(|_| SessionStartup {
            app: Some(PanelApp::choose_session()),
            selecting: false,
            path: None,
            error: None,
            opening: None,
            probing: None,
        });
        visual.update(|window, cx| startup.update(cx, |startup, cx| startup.record_to(window, cx)));
        visual.simulate_new_path_selection(|_| Some(path.clone()));
        visual.run_until_parked();
        cx.update(|cx| {
            assert_eq!(startup.read(cx).path.as_ref(), Some(&path));
            assert!(!path.exists());
            assert!(crate::connections::try_global(cx).is_none());
            assert!(
                cx.try_global::<crate::background_tasks::BackgroundTasks>()
                    .is_none()
            );
            startup.update(cx, |startup, cx| startup.temporary(cx));
            assert!(startup.read(cx).is_temporary());
        });
    }

    #[gpui::test]
    fn connecting_from_startup_uses_chosen_storage_and_opens_only_the_panel(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::sync::{Arc, Mutex};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selected.metor");
        let connected_to = Arc::new(Mutex::new(None));
        let captured = connected_to.clone();
        let target = crate::connections::ConnectionTarget::custom(
            "startup-storage-test",
            "Test",
            "",
            move |ctx: crate::connections::ConnectContext| {
                *captured.lock().unwrap() = Some(ctx.db.path.clone());
                crate::connections::Connected::default()
            },
        );
        let mut app = PanelApp::choose_session().connection(target.clone());
        cx.update(|cx| {
            cx.set_global(crate::theme::ActiveTheme(Arc::new(
                crate::theme::DARK.clone(),
            )));
            cx.set_global(crate::theme::FontSettings {
                family: "monospace".into(),
                config: Default::default(),
            });
            app.prepare_connections(false, cx);
        });
        let visual = cx.add_empty_window();
        let startup = visual.new(|_| SessionStartup {
            app: Some(app),
            selecting: false,
            path: Some(path.clone()),
            error: None,
            opening: None,
            probing: None,
        });
        visual.update(|window, cx| {
            startup.update(cx, |startup, cx| startup.connect(target, window, cx))
        });
        cx.update(|cx| {
            let store = crate::connections::try_global(cx).unwrap();
            assert_eq!(store.read(cx).active().len(), 1);
            assert_eq!(crate::workspace::panel_windows(cx).len(), 1);
            assert_eq!(cx.windows().len(), 1);
            assert_eq!(
                connected_to.lock().unwrap().as_ref(),
                Some(&path.canonicalize().unwrap().join("db"))
            );
            cx.shutdown();
            // Drop the map fetcher sender before the test executor drains
            // its blocking result receiver.
            cx.clear_globals();
        });
        assert!(path.join("db/db_state").exists());
        assert!(super::super::recording_lock(&path).is_ok());
    }
}
