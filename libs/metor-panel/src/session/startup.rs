//! Resolve storage before creating a DB, server, or connection worker.
use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Focusable as _, SharedString, Window, WindowOptions};

use super::SessionStorage;
use crate::PanelApp;

pub(crate) struct SessionStartup {
    app: Option<PanelApp>,
    selecting: bool,
    path: Option<PathBuf>,
    pub(crate) error: Option<SharedString>,
    opening: Option<Opening>,
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
    if let Err(error) = cx.open_window(WindowOptions::default(), |window, cx| {
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
        });
        let picker = cx.new(|cx| {
            crate::connections::ConnectionPicker::for_startup(store, startup.clone(), cx)
        });
        picker.focus_handle(cx).focus(window);
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
        let result = match self.path.clone() {
            Some(path) => SessionStorage::record_to(path),
            None => SessionStorage::create(),
        };
        match result {
            Ok((storage, db)) => {
                if !storage.is_temporary() {
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
        if self.selecting || self.opening.is_some() {
            return;
        }
        self.selecting = true;
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Open recording".into()),
        });
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
        if self.opening.is_some() || self.app.is_none() {
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
                gpui::Timer::after(std::time::Duration::from_millis(100)).await;
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
        if self.selecting || self.opening.is_some() || self.app.is_none() {
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
        self.selecting || self.opening.is_some()
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

    pub(crate) fn summary(&self) -> SharedString {
        if self.opening.is_some() {
            "Opening recording… Press Escape to cancel.".into()
        } else if self.selecting {
            "Choosing location…".into()
        } else if let Some(path) = &self.path {
            format!("Record to {}", path.display()).into()
        } else {
            "Temporary data is removed when you quit. Save a snapshot to keep it.".into()
        }
    }

    pub(crate) fn cancel_open(&self) {
        if let Some(opening) = &self.opening {
            opening
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
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
