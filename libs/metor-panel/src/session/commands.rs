use super::*;
use crate::inspector::{
    palette::{Category, InspectionItem, ItemRegistry},
    rows::{CommandRow, InspectorRow, TextRow},
};
use gpui::{Global, PromptLevel, SharedString, Window, actions};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    thread::JoinHandle,
};

actions!(
    metor_session,
    [SaveSnapshot, SaveSnapshotAs, OpenRecording, QuitPanel]
);
pub(crate) type OpenHandler = std::rc::Rc<dyn Fn(PathBuf) -> io::Result<()>>;
struct Export {
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    worker: JoinHandle<io::Result<Saved>>,
}
struct Saved {
    path: PathBuf,
    captured_at: i64,
    destination: archive::Destination,
}
struct SessionGlobal {
    storage: Option<SessionStorage>,
    db: Arc<DB>,
    selecting: bool,
    export: Option<Export>,
    saved: Option<Saved>,
    message: SharedString,
    open: OpenHandler,
}
impl Global for SessionGlobal {}

pub(crate) fn init(
    storage: Option<SessionStorage>,
    db: Arc<DB>,
    open: Option<OpenHandler>,
    cx: &mut App,
) {
    let message = if storage.as_ref().is_some_and(|s| s.source.is_some()) {
        "offline".into()
    } else {
        "No snapshot saved".into()
    };
    cx.set_global(SessionGlobal {
        storage,
        db,
        selecting: false,
        export: None,
        saved: None,
        message,
        open: open.unwrap_or_else(|| {
            std::rc::Rc::new(|path| {
                std::process::Command::new(std::env::current_exe()?)
                    .arg("--open")
                    .arg(path)
                    .spawn()?;
                Ok(())
            })
        }),
    });
    ItemRegistry::register(
        cx,
        Category::Command,
        Arc::new(|cx| {
            vec![
                InspectionItem::SubMenu {
                    label: "Session database".into(),
                    summary: storage_summary(cx),
                    build: Arc::new(rows),
                },
                InspectionItem::Command {
                    label: "Save snapshot…".into(),
                    callback: Arc::new(save),
                },
                InspectionItem::Command {
                    label: "Save snapshot as…".into(),
                    callback: Arc::new(save_as),
                },
                InspectionItem::Command {
                    label: "Open recording…".into(),
                    callback: Arc::new(open_recording),
                },
            ]
        }),
    );
}
pub(crate) fn take(cx: &mut App) -> Option<SessionStorage> {
    cx.try_global::<SessionGlobal>()?;
    cx.global_mut::<SessionGlobal>().storage.take()
}
fn storage_summary(cx: &App) -> SharedString {
    match cx
        .try_global::<SessionGlobal>()
        .and_then(|s| s.storage.as_ref())
    {
        Some(storage) if storage.source.is_some() => {
            format!("Offline: {}", storage.source.as_ref().unwrap().display()).into()
        }
        Some(SessionStorage {
            directory: StorageDirectory::Recording { path, .. },
            ..
        }) => format!("Recording to {}", path.display()).into(),
        Some(_) => "Temporary session".into(),
        None => "Caller-owned database".into(),
    }
}
pub(crate) fn status(cx: &App) -> SharedString {
    let Some(state) = cx.try_global::<SessionGlobal>() else {
        return "".into();
    };
    if let Some(export) = &state.export {
        format!(
            "Saving snapshot · {:.1} MiB",
            export.progress.load(Ordering::Relaxed) as f64 / 1048576.
        )
        .into()
    } else {
        state.message.clone()
    }
}
pub(crate) fn rows(cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![
        Box::new(TextRow::readonly("Storage".into(), storage_summary(cx))),
        Box::new(TextRow::readonly("Snapshot".into(), status(cx))),
    ];
    if let Some(state) = cx.try_global::<SessionGlobal>() {
        let gaps = state.db.snapshot_gaps().len();
        if gaps > 0 {
            rows.push(Box::new(TextRow::readonly(
                "Unavailable history".into(),
                format!("{gaps} remote ranges; snapshots include local data only").into(),
            )));
        }
        rows.push(Box::new(CommandRow::new("Save snapshot…", Arc::new(save))));
        rows.push(Box::new(CommandRow::new(
            "Save snapshot as…",
            Arc::new(save_as),
        )));
        rows.push(Box::new(CommandRow::new(
            "Open recording…",
            Arc::new(open_recording),
        )));
        if state.export.is_some() {
            rows.push(Box::new(CommandRow::new(
                "Cancel snapshot",
                Arc::new(|_, cx| {
                    if let Some(export) = &cx.global::<SessionGlobal>().export {
                        export.cancel.store(true, Ordering::Relaxed);
                    }
                }),
            )));
        }
    }
    rows
}
pub(crate) fn save(window: &mut Window, cx: &mut App) {
    choose_save(false, window, cx);
}
pub(crate) fn save_as(window: &mut Window, cx: &mut App) {
    choose_save(true, window, cx);
}
fn choose_save(save_as: bool, window: &mut Window, cx: &mut App) {
    let Some(state) = cx.try_global::<SessionGlobal>() else {
        return;
    };
    if state.selecting || state.export.is_some() {
        return;
    }
    let previous = (!save_as)
        .then(|| {
            state
                .saved
                .as_ref()
                .map(|s| (s.path.clone(), s.destination.clone()))
        })
        .flatten();
    let parent = state
        .saved
        .as_ref()
        .and_then(|s| s.path.parent())
        .map(Path::to_owned)
        .or_else(|| {
            cx.try_global::<crate::theme::FontSettings>()
                .and_then(|s| s.config.recording_parent.as_ref())
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir);
    cx.global_mut::<SessionGlobal>().selecting = true;
    let receiver = previous
        .is_none()
        .then(|| cx.prompt_for_new_path(&parent, Some("snapshot.metor")));
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let path = if let Some(receiver) = receiver {
            match receiver.await { Ok(Ok(Some(path))) => Some(path), Ok(Err(error)) => { let _ = cx.update(|cx| { cx.global_mut::<SessionGlobal>().message = error.to_string().into(); }); None }, _ => None }
        } else { previous.as_ref().map(|s| s.0.clone()) };
        let Some(path) = path else { let _ = cx.update(|cx| { cx.global_mut::<SessionGlobal>().selecting = false; }); return; };
        let prepared = cx.update(|cx| prepare_destination(path, cx)).map_err(io::Error::other).and_then(|r| r);
        let (path, expected, gaps) = match prepared {
            Ok(value) => value,
            Err(error) => { let _ = cx.update(|cx| { cx.global_mut::<SessionGlobal>().selecting = false; show_error(&handle, &error.to_string(), cx); }); return; }
        };
        let replace = !matches!(expected, archive::Destination::Missing) && previous.as_ref().is_none_or(|(_, old)| old != &expected);
        if replace || gaps > 0 {
            let detail = format!("{}{}Snapshots capture each stream independently. Data still queued for persistence is included in a later snapshot.",
                if replace { "This replaces the existing file. " } else { "" },
                if gaps > 0 { format!("{gaps} remote history ranges are unavailable and will be omitted. ") } else { String::new() });
            let prompt = handle.update(cx, |_, window, cx| window.prompt(PromptLevel::Warning, "Save snapshot?", Some(&detail), &["Save", "Cancel"], cx));
            if match prompt { Ok(prompt) => prompt.await.ok() != Some(0), Err(_) => true } {
                let _ = cx.update(|cx| { cx.global_mut::<SessionGlobal>().selecting = false; }); return;
            }
        }
        if handle.update(cx, |_, window, cx| begin_export(path, expected, window, cx)).is_err() {
            let _ = cx.update(|cx| { cx.global_mut::<SessionGlobal>().selecting = false; });
        }
    }).detach();
}
fn prepare_destination(
    path: PathBuf,
    cx: &App,
) -> io::Result<(PathBuf, archive::Destination, usize)> {
    let path = bundle_path(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let path = parent.canonicalize()?.join(path.file_name().unwrap());
    let state = cx.global::<SessionGlobal>();
    let storage_path = state
        .storage
        .as_ref()
        .map(|s| s.directory.path())
        .unwrap_or(&state.db.path)
        .canonicalize()?;
    if path.starts_with(storage_path) {
        return Err(io::Error::other(
            "Save the snapshot outside the session database",
        ));
    }
    if state
        .storage
        .as_ref()
        .and_then(|s| s.source.as_ref())
        .is_some_and(|source| source == &path)
    {
        return Err(io::Error::other(
            "Choose a new filename to preserve the opened recording",
        ));
    }
    let expected = archive::Destination::inspect(&path)?;
    Ok((path, expected, state.db.snapshot_gaps().len()))
}
fn begin_export(path: PathBuf, expected: archive::Destination, window: &mut Window, cx: &mut App) {
    let workspace =
        crate::workspace::serialize_workspace(crate::workspace::window_layout(window, cx), cx);
    let state = cx.global_mut::<SessionGlobal>();
    state.selecting = false;
    let db = state.db.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let progress = Arc::new(AtomicU64::new(0));
    let worker_progress = progress.clone();
    let worker = std::thread::spawn(move || {
        let snapshot = db.snapshot().map_err(io::Error::other)?;
        archive::save(
            &snapshot,
            workspace.as_deref(),
            &path,
            &expected,
            &worker_cancel,
            &worker_progress,
        )?;
        let destination = archive::Destination::inspect(&path)?;
        Ok(Saved {
            path,
            captured_at: snapshot.captured_at.0,
            destination,
        })
    });
    state.export = Some(Export {
        cancel,
        progress,
        worker,
    });
    cx.spawn(async |cx| {
        loop {
            futures_lite::future::yield_now().await;
            gpui::Timer::after(std::time::Duration::from_millis(100)).await;
            let keep_polling = cx
                .update(|cx| {
                    let state = cx.global_mut::<SessionGlobal>();
                    let Some(export) = &state.export else {
                        return false;
                    };
                    if export.worker.is_finished() {
                        finish_export(cx);
                        return false;
                    }
                    cx.refresh_windows();
                    true
                })
                .unwrap_or(false);
            if !keep_polling {
                break;
            }
        }
    })
    .detach();
    cx.refresh_windows();
}
/// Quit observers run synchronously: wait for the exporter before any source
/// directory is removed, regardless of GPUI's async quit timeout.
pub(crate) fn finish_export(cx: &mut App) {
    if cx.try_global::<SessionGlobal>().is_none() {
        return;
    }
    let state = cx.global_mut::<SessionGlobal>();
    let Some(export) = state.export.take() else {
        return;
    };
    match export
        .worker
        .join()
        .map_err(|_| io::Error::other("Snapshot worker panicked"))
        .and_then(|r| r)
    {
        Ok(saved) => {
            let when = jiff::Timestamp::from_microsecond(saved.captured_at)
                .map(|t| t.to_string())
                .unwrap_or_else(|_| saved.captured_at.to_string());
            state.message = format!("Snapshot saved at {when} · {}", saved.path.display()).into();
            state.saved = Some(saved);
        }
        Err(error) => {
            tracing::error!(%error, "snapshot save failed");
            state.message = format!("Snapshot: {error}").into();
        }
    }
    cx.refresh_windows();
}
pub(crate) fn request_quit(window: &mut Window, cx: &mut App) {
    if cx
        .try_global::<SessionGlobal>()
        .is_none_or(|s| s.export.is_none())
    {
        cx.quit();
        return;
    }
    let prompt = window.prompt(
        PromptLevel::Info,
        "A snapshot is being saved",
        Some("Wait for the save to finish, or cancel it before quitting."),
        &["Wait and quit", "Cancel save and quit", "Keep working"],
        cx,
    );
    cx.spawn(async move |cx| {
        let Ok(answer) = prompt.await else {
            return;
        };
        let _ = cx.update(|cx| {
            if answer == 2 {
                return;
            }
            if answer == 1
                && let Some(export) = &cx.global::<SessionGlobal>().export
            {
                export.cancel.store(true, Ordering::Relaxed);
            }
            cx.quit();
        });
    })
    .detach();
}
pub(crate) fn open_recording(window: &mut Window, cx: &mut App) {
    let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: true,
        multiple: false,
        prompt: Some("Open recording".into()),
    });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        let _ = cx.update(|cx| {
            let result = (cx.global::<SessionGlobal>().open)(path);
            if let Err(error) = result {
                show_error(&handle, &error.to_string(), cx);
            }
        });
    })
    .detach();
}
fn show_error(window: &gpui::AnyWindowHandle, detail: &str, cx: &mut App) {
    cx.global_mut::<SessionGlobal>().message = detail.to_owned().into();
    let _ = window.update(cx, |_, window, cx| {
        drop(window.prompt(
            PromptLevel::Critical,
            "Could not save or open recording",
            Some(detail),
            &["OK"],
            cx,
        ));
    });
    cx.refresh_windows();
}
pub(crate) fn save_workspace(
    extra: Option<(gpui::WindowId, crate::workspace::WindowLayout)>,
    cx: &mut App,
) {
    let path = cx
        .try_global::<SessionGlobal>()
        .and_then(|s| s.storage.as_ref())
        .and_then(|s| match &s.directory {
            StorageDirectory::Recording { path, .. } => Some(path.clone()),
            _ => None,
        });
    let Some(path) = path else {
        return;
    };
    let Some(workspace) = crate::workspace::serialize_workspace(extra, cx) else {
        return;
    };
    let result = (|| -> io::Result<()> {
        use std::io::Write as _;
        let mut staging = tempfile::NamedTempFile::new_in(&path)?;
        staging.write_all(workspace.as_bytes())?;
        staging.as_file().sync_all()?;
        staging
            .persist(path.join("workspace.json"))
            .map_err(|e| e.error)?;
        sync_directory(&path)
    })();
    if let Err(error) = result {
        tracing::error!(%error, "could not save recording workspace");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::populate;

    #[gpui::test]
    fn cancelling_save_keeps_the_live_session_and_no_destination(cx: &mut gpui::TestAppContext) {
        let (storage, db) = SessionStorage::create().unwrap();
        cx.update(|cx| {
            ItemRegistry::init(cx);
            init(Some(storage), db.clone(), None, cx);
        });
        let visual = cx.add_empty_window();
        visual.update(save);
        assert!(visual.did_prompt_for_new_path());
        visual.simulate_new_path_selection(|_| None);
        visual.run_until_parked();
        cx.update(|cx| {
            let state = cx.global::<SessionGlobal>();
            assert!(!state.selecting);
            assert!(state.export.is_none());
            assert!(state.saved.is_none());
        });
        assert!(db.path.exists());
    }

    #[gpui::test]
    fn quit_joins_export_before_cleaning_source_and_save_reuses_its_destination(
        cx: &mut gpui::TestAppContext,
    ) {
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("snapshot.metor");
        let (storage, db) = SessionStorage::create().unwrap();
        populate(&db);
        db.flush().unwrap();
        cx.update(|cx| {
            ItemRegistry::init(cx);
            init(Some(storage), db.clone(), None, cx);
            crate::app::register_shutdown(db.clone(), Default::default(), cx);
        });
        let visual = cx.add_empty_window();
        visual.update(save);
        visual.simulate_new_path_selection(|_| Some(destination.clone()));
        visual.run_until_parked();
        visual.update(|_, cx| finish_export(cx));
        visual.update(|_, cx| {
            assert_eq!(
                cx.global::<SessionGlobal>().saved.as_ref().unwrap().path,
                destination.canonicalize().unwrap()
            );
        });
        visual.update(save);
        visual.run_until_parked();
        visual.update(|_, cx| {
            assert!(cx.global::<SessionGlobal>().export.is_some());
        });
        cx.update(|cx| cx.shutdown());
        assert!(!db.path.exists());
        let (copy, copy_db, _) =
            SessionStorage::import(destination, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            copy_db.with_state(|s| s
                .get_component(metor_proto::types::ComponentId(42))
                .unwrap()
                .time_series
                .latest()
                .unwrap()
                .timestamp()
                .0),
            123
        );
        copy.finish(&copy_db, Ok(())).unwrap();
    }

    #[test]
    fn imported_runtime_stops_before_an_abandoned_working_copy_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(root.path().join("db")).unwrap());
        populate(&db);
        db.flush().unwrap();
        let (copy, copy_db, _) =
            SessionStorage::import(db.path.clone(), &AtomicBool::new(false)).unwrap();
        let path = copy.directory.path().to_owned();
        drop(copy);
        drop(copy_db);
        assert!(!path.exists());
        assert!(db.path.exists());
    }
}
