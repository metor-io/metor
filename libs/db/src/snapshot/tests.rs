use super::*;
use crate::{ComponentSchema, manifest::SpanSource};
use metor_proto::types::{ComponentId, PrimType};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn database(path: &Path) -> Arc<DB> {
    let path = path.to_owned();
    std::thread::spawn(move || {
        stellarator::run(|| async move {
            let db = Arc::new(DB::create(path).unwrap());
            db.with_state_mut(|s| {
                s.insert_engine_component(
                    ComponentId(42),
                    ComponentSchema::new(PrimType::I64, &[][..]),
                    &db.path,
                )
                .unwrap();
                s.get_or_insert_msg_log([7, 0], &db.path).unwrap();
            });
            db
        })
    })
    .join()
    .unwrap()
}
fn bytes(snapshot: &Snapshot) -> Vec<(String, Vec<u8>)> {
    snapshot
        .files()
        .iter()
        .map(|f| (f.path.clone(), f.parts().concat()))
        .collect()
}
fn check_records(snapshot: &Snapshot) {
    for file in snapshot.files() {
        if let Some(prefix) = file.path.strip_suffix("/index") {
            let data = snapshot
                .files()
                .iter()
                .find(|f| f.path == format!("{prefix}/data"))
                .unwrap();
            assert_eq!(file.parts()[1], data.parts()[1]);
        }
        if let Some(prefix) = file.path.strip_suffix("/timestamps") {
            let offsets = snapshot
                .files()
                .iter()
                .find(|f| f.path == format!("{prefix}/offsets"))
                .unwrap();
            let payload = snapshot
                .files()
                .iter()
                .find(|f| f.path == format!("{prefix}/data_log"))
                .unwrap();
            assert_eq!(file.parts()[1].len() * 2, offsets.parts()[1].len());
            for (ts, descriptor) in file.parts()[1]
                .chunks_exact(8)
                .zip(offsets.parts()[1].chunks_exact(16))
            {
                let len = u32::from_le_bytes(descriptor[..4].try_into().unwrap()) as usize;
                if len > 12 {
                    let offset = u32::from_le_bytes(descriptor[12..16].try_into().unwrap()) as usize;
                    assert_eq!(&payload.parts()[1][offset..offset + 8], ts);
                    assert!(offset + len <= payload.parts()[1].len());
                } else {
                    assert_eq!(&descriptor[4..12], ts);
                }
            }
        }
    }
}

#[test]
fn concurrent_capture_contains_complete_records_and_frozen_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    let component = db.with_state(|s| s.get_component(ComponentId(42)).unwrap().clone());
    component
        .time_series
        .set_max_node_age(Duration::from_micros(100));
    let mut writer = component.time_series.writer().unwrap();
    let log = db.with_state_mut(|s| s.get_or_insert_msg_log([7, 0], &db.path).unwrap().clone());
    let done = Arc::new(AtomicBool::new(false));
    let thread_done = done.clone();
    let producer = std::thread::spawn(move || {
        for i in 1i64..=1000 {
            writer.push_buf(Timestamp(i), &i.to_le_bytes()).unwrap();
            let mut msg = i.to_le_bytes().to_vec();
            if i % 3 != 0 {
                msg.extend_from_slice(b" payload outside inline storage");
            }
            log.push(Timestamp(i), &msg).unwrap();
            log.flush_pending().unwrap();
            std::thread::yield_now();
        }
        thread_done.store(true, Ordering::Release);
    });
    let mut snapshots = Vec::new();
    while !done.load(Ordering::Acquire) {
        let snapshot = db.snapshot().unwrap();
        check_records(&snapshot);
        let captured = bytes(&snapshot);
        snapshots.push((snapshot, captured));
        std::thread::sleep(Duration::from_millis(1));
    }
    producer.join().unwrap();
    check_records(&db.snapshot().unwrap());
    for (snapshot, captured) in snapshots {
        assert_eq!(bytes(&snapshot), captured);
    }
    assert_eq!(
        component.time_series.latest().unwrap().timestamp(),
        Timestamp(1000)
    );
}

#[test]
fn pinned_nodes_survive_purge_and_capture_reports_local_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    let component = db.with_state(|s| s.get_component(ComponentId(42)).unwrap().clone());
    let series = &component.time_series;
    series.set_max_node_age(Duration::from_micros(1));
    let mut writer = series.writer().unwrap();
    writer.push_buf(Timestamp(1), &1i64.to_le_bytes()).unwrap();
    writer
        .push_buf(Timestamp(10), &10i64.to_le_bytes())
        .unwrap();
    series.seal_rolled_nodes().unwrap();
    let snapshot = db.snapshot().unwrap();
    let captured = bytes(&snapshot);
    series.mark_acked(Timestamp(1)).unwrap();
    assert!(series.purge_span(Timestamp(1)).unwrap());
    assert_eq!(bytes(&snapshot), captured);
    assert_eq!(db.snapshot().unwrap().gaps[0].start, 1);
    series
        .install_samples(
            8,
            [(Timestamp(1), &1i64.to_le_bytes()[..])],
            SpanSource::RemoteFetch,
        )
        .unwrap();
    assert_eq!(bytes(&snapshot), captured);
    assert!(db.snapshot().unwrap().gaps.is_empty());
}
