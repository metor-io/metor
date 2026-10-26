//! The fixture and table builders the db benches share.
//!
//! Components are created on a host thread running a stellarator executor,
//! as a server connection creates them, while the benches ingest from their
//! own threads.

// Each bench target uses its own subset.
#![allow(dead_code)]

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use metor_db::{Component, DB};
use metor_proto::{
    types::{ComponentId, PacketId, PrimType},
    vtable::builder::{component, raw_field, raw_table, schema, timestamp, vtable},
};
use metor_proto_wkt::VTableMsg;

pub const THREADS: [usize; 3] = [1, 4, 8];

/// A db whose components live on a host executor thread.
pub struct Fixture {
    pub db: Arc<DB>,
    stop: Arc<AtomicBool>,
    host: Option<thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// Creates a db and runs `setup` on the host thread before returning.
    pub fn new(setup: impl FnOnce(&DB) + Send + 'static) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(dir.path().to_path_buf()).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let host = {
            let db = db.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                stellarator::run(|| async move {
                    setup(&db);
                    ready_tx.send(()).unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        stellarator::sleep(Duration::from_millis(10)).await;
                    }
                })
            })
        };
        ready_rx.recv().unwrap();
        Fixture {
            db,
            stop,
            host: Some(host),
            _dir: dir,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(host) = self.host.take() {
            host.join().unwrap();
        }
    }
}

/// The id of producer `producer`'s static table.
pub fn static_id(producer: usize) -> PacketId {
    (0x100 + producer as u16).to_le_bytes()
}

/// Registers a table of `fields` `f64`s under a leading timestamp, with
/// components named `bench.<producer>.<field>`.
pub fn insert_static(db: &DB, producer: usize, fields: usize) {
    let fields = (0..fields).map(|field| {
        raw_field(
            8 + 8 * field as u32,
            8,
            timestamp(
                raw_table(0, 8),
                schema(
                    PrimType::F64,
                    &[],
                    component(format!("bench.{producer}.{field}").as_str()),
                ),
            ),
        )
    });
    db.insert_vtable(VTableMsg {
        id: static_id(producer),
        vtable: vtable(fields),
    })
    .unwrap();
}

/// A static table of `fields` values; its timestamp is set by [`stamp`].
pub fn static_table(fields: usize) -> Vec<u8> {
    let mut table = vec![0; 8];
    for field in 0..fields {
        table.extend_from_slice(&(field as f64).to_le_bytes());
    }
    table
}

/// Stamps `table`'s leading timestamp, which must rise per component.
pub fn stamp(table: &mut [u8], ts: i64) {
    table[..8].copy_from_slice(&ts.to_le_bytes());
}

/// Runs `op(thread, i)` for `i in 0..iters` on each of `threads` threads at
/// once, and returns the wall time they took together.
pub fn run_threads(threads: usize, iters: u64, op: impl Fn(usize, u64) + Sync) -> Duration {
    let start = Barrier::new(threads + 1);
    thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (start, op) = (&start, &op);
                s.spawn(move || {
                    start.wait();
                    for i in 0..iters {
                        op(t, i);
                    }
                })
            })
            .collect();
        start.wait();
        let began = Instant::now();
        for handle in handles {
            handle.join().unwrap();
        }
        began.elapsed()
    })
}

/// Each thread's last timestamp, so samples keep rising across criterion's
/// repeated `iter_custom` calls.
pub struct Clocks(Vec<std::sync::atomic::AtomicI64>);

impl Clocks {
    pub fn new(threads: usize) -> Self {
        Clocks((0..threads).map(|_| Default::default()).collect())
    }

    pub fn tick(&self, thread: usize) -> i64 {
        self.0[thread].fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// The component holding field `field` of producer `producer`'s table.
pub fn get_component(db: &DB, producer: usize, field: usize) -> Component {
    let id = ComponentId::new(&format!("bench.{producer}.{field}"));
    db.with_state(|s| s.get_component(id).cloned())
        .expect("component registered")
}
