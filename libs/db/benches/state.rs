//! Contention on the db's component, metadata and message-log state.
//!
//! Components are created on a host thread running a stellarator executor,
//! as the server creates them, so their persist tasks drain the WAL while the
//! benches ingest. Benches that measure several threads report per-op time
//! for one op on each thread ([`Throughput::Elements`] of the thread count).

use std::{
    hint::black_box,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use metor_db::{Component, DB};
use metor_proto::{
    types::{ComponentId, PacketId, PrimType},
    vtable::builder::{
        FieldBuilder, component, map, path_component, raw_field, raw_table, schema, timestamp,
        vtable,
    },
};
use metor_proto_wkt::{ComponentMetadata, VTableMsg};

const THREADS: [usize; 3] = [1, 4, 8];

/// A db whose components live on a host executor thread.
struct Fixture {
    db: Arc<DB>,
    stop: Arc<AtomicBool>,
    host: Option<thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// Creates a db and runs `setup` on the host thread before returning.
    fn new(setup: impl FnOnce(&DB) + Send + 'static) -> Self {
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
fn static_id(producer: usize) -> PacketId {
    (0x100 + producer as u16).to_le_bytes()
}

/// Registers a table of `fields` `f64`s under a leading timestamp, with
/// components named `bench.<producer>.<field>`.
fn insert_static(db: &DB, producer: usize, fields: usize) {
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
fn static_table(fields: usize) -> Vec<u8> {
    let mut table = vec![0; 8];
    for field in 0..fields {
        table.extend_from_slice(&(field as f64).to_le_bytes());
    }
    table
}

/// Stamps `table`'s leading timestamp, which must rise per component.
fn stamp(table: &mut [u8], ts: i64) {
    table[..8].copy_from_slice(&ts.to_le_bytes());
}

/// Runs `op(thread, i)` for `i in 0..iters` on each of `threads` threads at
/// once, and returns the wall time they took together.
fn run_threads(threads: usize, iters: u64, op: impl Fn(usize, u64) + Sync) -> Duration {
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
struct Clocks(Vec<std::sync::atomic::AtomicI64>);

impl Clocks {
    fn new(threads: usize) -> Self {
        Clocks((0..threads).map(|_| Default::default()).collect())
    }

    fn tick(&self, thread: usize) -> i64 {
        self.0[thread].fetch_add(1, Ordering::Relaxed) + 1
    }
}

fn ingest_static(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingest_static");
    for fields in [1, 16, 128] {
        let fixture = Fixture::new(move |db| insert_static(db, 0, fields));
        let mut table = static_table(fields);
        let mut ts = 0;
        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("fields", fields), |b| {
            b.iter(|| {
                ts += 1;
                stamp(&mut table, ts);
                fixture.db.ingest_table(static_id(0), &table).unwrap();
            })
        });
    }
    group.finish();
}

/// Threads ingest their own tables at once: no shared component, only the
/// shared state.
fn ingest_static_contended(c: &mut Criterion) {
    const FIELDS: usize = 16;
    let mut group = c.benchmark_group("ingest_static/contended");
    for threads in THREADS {
        let fixture = Fixture::new(move |db| {
            for producer in 0..threads {
                insert_static(db, producer, FIELDS);
            }
        });
        let clocks = Clocks::new(threads);
        group.throughput(Throughput::Elements(threads as u64));
        group.bench_function(BenchmarkId::new("threads", threads), |b| {
            b.iter_custom(|iters| {
                run_threads(threads, iters, |t, _| {
                    let mut table = static_table(FIELDS);
                    stamp(&mut table, clocks.tick(t));
                    fixture.db.ingest_table(static_id(t), &table).unwrap();
                })
            })
        });
    }
    group.finish();
}

/// As [`ingest_static_contended`], while another thread rewrites component
/// metadata as fast as it can, as a panel editing labels or a producer
/// re-announcing does.
fn ingest_static_metadata_churn(c: &mut Criterion) {
    const FIELDS: usize = 16;
    let mut group = c.benchmark_group("ingest_static/metadata_churn");
    // An op waits out metadata writes, so few fit in the default time.
    group.sample_size(20);
    for threads in THREADS {
        let fixture = Fixture::new(move |db| {
            for producer in 0..threads {
                insert_static(db, producer, FIELDS);
            }
        });
        let clocks = Clocks::new(threads);
        group.throughput(Throughput::Elements(threads as u64));
        group.bench_function(BenchmarkId::new("threads", threads), |b| {
            b.iter_custom(|iters| {
                let stop = AtomicBool::new(false);
                thread::scope(|s| {
                    s.spawn(|| {
                        let mut n = 0u64;
                        while !stop.load(Ordering::Relaxed) {
                            let name = format!("bench.churn.{}", n % 64);
                            fixture
                                .db
                                .set_component_metadata(ComponentMetadata::from(name.as_str()))
                                .unwrap();
                            n += 1;
                        }
                    });
                    let elapsed = run_threads(threads, iters, |t, _| {
                        let mut table = static_table(FIELDS);
                        stamp(&mut table, clocks.tick(t));
                        fixture.db.ingest_table(static_id(t), &table).unwrap();
                    });
                    stop.store(true, Ordering::Relaxed);
                    elapsed
                })
            })
        });
    }
    group.finish();
}

const DYNAMIC_ID: PacketId = [0x00, 0x02];

fn process_members() -> Vec<FieldBuilder> {
    vec![
        raw_field(0, 8, schema(PrimType::U64, &[], path_component("pid"))),
        raw_field(8, 8, schema(PrimType::F64, &[], path_component("cpu"))),
    ]
}

/// A `processes` map table of `keys` entries, each `(pid, cpu)`.
fn map_table(keys: usize) -> Vec<u8> {
    let names: Vec<String> = (0..keys).map(|k| format!("p{k}")).collect();
    let mut table = vec![0; 8];
    table.extend_from_slice(&16u32.to_le_bytes());
    table.extend_from_slice(&((keys * 24) as u32).to_le_bytes());
    let mut key_off = 16 + keys * 24;
    for (k, name) in names.iter().enumerate() {
        table.extend_from_slice(&(key_off as u32).to_le_bytes());
        table.extend_from_slice(&(name.len() as u32).to_le_bytes());
        table.extend_from_slice(&(k as u64).to_le_bytes());
        table.extend_from_slice(&0.5f64.to_le_bytes());
        key_off += name.len();
    }
    for name in &names {
        table.extend_from_slice(name.as_bytes());
    }
    table
}

/// A dynamic table whose keys all exist: the steady state of a process list.
fn ingest_dynamic(c: &mut Criterion) {
    const KEYS: usize = 32;
    let fixture = Fixture::new(|db| {
        db.insert_vtable(VTableMsg {
            id: DYNAMIC_ID,
            vtable: vtable([raw_field(
                8,
                8,
                timestamp(raw_table(0, 8), map("processes", process_members(), 24, 8)),
            )]),
        })
        .unwrap();
        // Realize every member here, so their persist tasks run on the host.
        let mut table = map_table(KEYS);
        stamp(&mut table, 1);
        db.ingest_table(DYNAMIC_ID, &table).unwrap();
    });
    let mut table = map_table(KEYS);
    let mut ts = 1;
    let mut group = c.benchmark_group("ingest_dynamic");
    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("known_keys", KEYS), |b| {
        b.iter(|| {
            ts += 1;
            stamp(&mut table, ts);
            fixture.db.ingest_table(DYNAMIC_ID, &table).unwrap();
        })
    });
    group.finish();
}

fn push_msg_contended(c: &mut Criterion) {
    let mut group = c.benchmark_group("push_msg/contended");
    let msg = [0u8; 64];
    for threads in THREADS {
        let fixture = Fixture::new(move |db| {
            for t in 0..threads {
                let id = (0x200 + t as u16).to_le_bytes();
                db.push_msg(metor_proto::types::Timestamp(0), id, &msg)
                    .unwrap();
            }
        });
        let clocks = Clocks::new(threads);
        group.throughput(Throughput::Elements(threads as u64));
        group.bench_function(BenchmarkId::new("threads", threads), |b| {
            b.iter_custom(|iters| {
                run_threads(threads, iters, |t, _| {
                    let id = (0x200 + t as u16).to_le_bytes();
                    let ts = metor_proto::types::Timestamp(clocks.tick(t));
                    // A full WAL refuses the push, as the persister falls
                    // behind; that is backpressure, not the state under test.
                    match fixture.db.push_msg(ts, id, &msg) {
                        Ok(()) | Err(metor_db::Error::MapOverflow) => {}
                        Err(err) => panic!("{err}"),
                    }
                })
            })
        });
    }
    group.finish();
}

/// An owned value from a lookup that returns either a borrow or a clone, so
/// [`lookup`] costs what its callers pay on either side of the state change.
trait Owned<T> {
    fn owned(self) -> T;
}

impl<T: Clone> Owned<T> for &T {
    fn owned(self) -> T {
        self.clone()
    }
}

impl<T> Owned<T> for T {
    fn owned(self) -> T {
        self
    }
}

/// Point lookups into a db of 1024 components, cloned out as the panel
/// does per frame.
fn lookup(c: &mut Criterion) {
    const PRODUCERS: usize = 8;
    const FIELDS: usize = 128;
    let fixture = Fixture::new(|db| {
        for producer in 0..PRODUCERS {
            insert_static(db, producer, FIELDS);
        }
    });
    let ids: Vec<ComponentId> = (0..PRODUCERS * FIELDS)
        .map(|i| ComponentId::new(&format!("bench.{}.{}", i / FIELDS, i % FIELDS)))
        .collect();
    let mut group = c.benchmark_group("lookup");
    for threads in THREADS {
        group.throughput(Throughput::Elements(threads as u64));
        group.bench_function(BenchmarkId::new("get_component", threads), |b| {
            b.iter_custom(|iters| {
                run_threads(threads, iters, |t, i| {
                    let id = ids[(i as usize * 7 + t * 131) % ids.len()];
                    black_box(
                        fixture
                            .db
                            .with_state(|s| s.get_component(id).map(Owned::<Component>::owned)),
                    );
                })
            })
        });
        group.bench_function(BenchmarkId::new("get_component_metadata", threads), |b| {
            b.iter_custom(|iters| {
                run_threads(threads, iters, |t, i| {
                    let id = ids[(i as usize * 7 + t * 131) % ids.len()];
                    black_box(fixture.db.with_state(|s| {
                        s.get_component_metadata(id)
                            .map(Owned::<ComponentMetadata>::owned)
                    }));
                })
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    ingest_static,
    ingest_static_contended,
    ingest_static_metadata_churn,
    ingest_dynamic,
    push_msg_contended,
    lookup
);
criterion_main!(benches);
