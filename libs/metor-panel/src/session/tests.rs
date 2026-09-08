use super::*;
use metor_db::ComponentSchema;
use metor_proto::types::{ComponentId, PrimType, Timestamp};

pub(super) fn populate(db: &Arc<DB>) {
    // Return without yielding: persistence tasks never get polled. A save
    // must still retain this tail after the executor and its tasks are gone.
    let db = db.clone();
    std::thread::spawn(move || {
        stellarator::run(|| async move {
            db.with_state_mut(|s| {
                s.insert_component(
                    ComponentId(42),
                    ComponentSchema::new(PrimType::F64, &[][..]),
                    &db.path,
                )
            })
            .unwrap();
            let component = db
                .with_state(|s| s.get_component(ComponentId(42)).cloned())
                .unwrap();
            component
                .push_buf(Timestamp(123), &1.25f64.to_le_bytes())
                .unwrap();
            db.push_msg(
                Timestamp(124),
                [7, 0],
                b"a message longer than the inline buffer",
            )
            .unwrap();
        })
    })
    .join()
    .unwrap();
}

pub(super) fn assert_bundle(path: &Path) {
    let path = path.to_owned();
    std::thread::spawn(move || {
        stellarator::run(|| async move {
            let db = DB::open(path).unwrap();
            db.with_state_mut(|s| {
                let component = s.get_component(ComponentId(42)).unwrap();
                let sample = component.time_series.latest().unwrap();
                assert_eq!(sample.timestamp(), Timestamp(123));
                assert_eq!(sample.data(), &1.25f64.to_le_bytes());
                assert_eq!(
                    component
                        .time_series
                        .list
                        .iter()
                        .map(|n| n.timestamps().len())
                        .sum::<usize>(),
                    1
                );
                let log = s.get_or_insert_msg_log([7, 0], &db.path).unwrap();
                let message = log.latest().unwrap();
                assert_eq!(message.timestamp(), Timestamp(124));
                assert_eq!(
                    message.data().unwrap(),
                    b"a message longer than the inline buffer"
                );
                assert_eq!(log.list.iter().map(|n| n.msg_count()).sum::<usize>(), 1);
            });
        })
    })
    .join()
    .unwrap();
}

#[test]
fn temporary_sessions_are_isolated_and_removed() {
    let (session, db) = SessionStorage::create().unwrap();
    let (other, other_db) = SessionStorage::create().unwrap();
    assert_ne!(db.path, other_db.path);
    session.finish(&db, Ok(())).unwrap();
    assert!(!db.path.exists());
    assert!(other_db.path.exists());
    other.finish(&other_db, Ok(())).unwrap();
}

#[test]
fn persistent_recording_retains_tail_and_import_never_changes_source() {
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("recording.metor");
    let (session, db) = SessionStorage::record_to(path.clone()).unwrap();
    populate(&db);
    assert!(recording_lock(&path).is_err());
    assert!(SessionStorage::import(path.clone(), &AtomicBool::new(false)).is_err());
    session.finish(&db, Ok(())).unwrap();
    assert_bundle(&path.join("db"));
    let before = fs::read(path.join("db/db_state")).unwrap();
    let (copy, copy_db, _) = SessionStorage::import(path.clone(), &AtomicBool::new(false)).unwrap();
    assert_ne!(copy_db.path, db.path);
    assert_eq!(
        copy_db.with_state(|s| s
            .get_component(ComponentId(42))
            .unwrap()
            .time_series
            .latest()
            .unwrap()
            .timestamp()),
        Timestamp(123)
    );
    let copy_path = copy.directory.path().to_owned();
    copy.finish(&copy_db, Ok(())).unwrap();
    assert!(!copy_path.exists());
    assert_eq!(fs::read(path.join("db/db_state")).unwrap(), before);
    assert!(recording_lock(&path).is_ok());
}

#[test]
fn existing_paths_are_never_overwritten_by_recording_creation() {
    let target = tempfile::tempdir().unwrap();
    let file = target.path().join("file.metor");
    fs::write(&file, b"existing snapshot").unwrap();
    assert!(SessionStorage::record_to(file.clone()).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"existing snapshot");
    let directory = target.path().join("directory.metor");
    fs::create_dir(&directory).unwrap();
    assert!(SessionStorage::record_to(directory.clone()).is_err());
    assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn recording_creation_and_import_reject_symlinks() {
    let target = tempfile::tempdir().unwrap();
    let link = target.path().join("link.metor");
    std::os::unix::fs::symlink(target.path(), &link).unwrap();
    assert!(SessionStorage::record_to(link.clone()).is_err());
    assert!(SessionStorage::import(link, &AtomicBool::new(false)).is_err());
}

#[test]
fn shutdown_failure_retains_recovery_data() {
    let (session, db) = SessionStorage::create().unwrap();
    let err = session
        .finish(&db, Err(io::Error::other("worker failed")))
        .unwrap_err();
    assert_eq!(err.recovery, db.path);
    assert!(db.path.join("db_state").exists());
    fs::remove_dir_all(&db.path).unwrap();
}

#[test]
fn caller_owned_and_unstarted_persistent_databases_are_retained() {
    let target = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(target.path().join("db")).unwrap());
    drop(crate::PanelApp::new(db.clone()));
    assert!(db.path.join("db_state").exists());
    let path = target.path().join("recording.metor");
    drop(crate::PanelApp::record_to(path.clone()).unwrap());
    assert!(path.join("db/db_state").exists());
    assert!(recording_lock(&path).is_ok());
}

#[gpui::test]
fn quit_removes_temporary_storage_but_window_close_keeps_it(cx: &mut gpui::TestAppContext) {
    let (session, db) = SessionStorage::create().unwrap();
    let path = db.path.clone();
    cx.update(|cx| {
        crate::inspector::palette::ItemRegistry::init(cx);
        init(Some(session), db.clone(), None, cx);
        crate::app::register_shutdown(db, Default::default(), cx);
    });
    cx.add_empty_window()
        .update(|window, _| window.remove_window());
    assert!(path.exists());
    cx.update(|cx| cx.shutdown());
    assert!(!path.exists());
}
