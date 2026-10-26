//! The per-sample paths: ingest through the WAL into storage, the live
//! stream tick, and reads of a component's history.

mod common;

use std::{hint::black_box, thread, time::Duration};

use common::{Fixture, get_component, insert_static, stamp, static_id, static_table};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use metor_db::{Component, time_series::TimeSeries};
use metor_proto::types::Timestamp;

const FIELDS: usize = 16;

/// Tables ingested between waits for persistence. A component's WAL holds 512
/// `f64` samples, and a full WAL drops a push, so each batch must fit.
const BATCH: u64 = 256;

/// Spins until every one of `components` has persisted its sample at `ts`.
fn wait_persisted(components: &[Component], ts: i64) {
    for component in components {
        while component
            .time_series
            .latest()
            .is_none_or(|latest| latest.timestamp().0 < ts)
        {
            thread::yield_now();
        }
    }
}

/// Ingests a table of [`FIELDS`] values and waits for it to be written to
/// storage, while `subscribers` live readers tail each component's WAL as
/// streams to the panel do. Unlike `state`'s ingest benches, which can
/// outrun persistence, every sample here reaches storage.
fn ingest_persisted(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingest_persisted");
    for subscribers in [0, 4, 16] {
        let fixture = Fixture::new(move |db| {
            insert_static(db, 0, FIELDS);
            for field in 0..FIELDS {
                for _ in 0..subscribers {
                    let mut reader = get_component(db, 0, field).wal.reader();
                    stellarator::spawn(async move {
                        loop {
                            drop(reader.next().await);
                        }
                    });
                }
            }
        });
        let components: Vec<_> = (0..FIELDS)
            .map(|field| get_component(&fixture.db, 0, field))
            .collect();
        let mut table = static_table(FIELDS);
        let mut ts = 0;
        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("subscribers", subscribers), |b| {
            b.iter_custom(|iters| {
                let start = std::time::Instant::now();
                for i in 0..iters {
                    ts += 1;
                    stamp(&mut table, ts);
                    fixture.db.ingest_table(static_id(0), &table).unwrap();
                    if i % BATCH == BATCH - 1 {
                        wait_persisted(&components, ts);
                    }
                }
                wait_persisted(&components, ts);
                start.elapsed()
            })
        });
    }
    group.finish();
}

/// A series of `samples` `f64`s, one per microsecond, split across `nodes`
/// nodes.
fn series(dir: &std::path::Path, samples: i64, nodes: i64) -> TimeSeries {
    let series = TimeSeries::create(dir).unwrap();
    series.set_max_node_age(Duration::from_micros((samples / nodes) as u64 - 1));
    let mut writer = series.writer().unwrap();
    for ts in 0..samples {
        writer
            .push_buf(Timestamp(ts), &(ts as f64).to_le_bytes())
            .unwrap();
    }
    series
}

/// Persistence's append of one sample to storage.
fn time_series_write(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let series = TimeSeries::create(dir.path()).unwrap();
    let mut writer = series.writer().unwrap();
    let mut ts = 0;
    let mut group = c.benchmark_group("time_series");
    group.throughput(Throughput::Elements(1));
    group.bench_function("write", |b| {
        b.iter(|| {
            ts += 1;
            writer
                .push_buf(Timestamp(ts), &(ts as f64).to_le_bytes())
                .unwrap();
        })
    });
    group.finish();
}

/// Point reads of a million-sample series, as plots, cursors and the live
/// stream make them.
fn time_series_read(c: &mut Criterion) {
    const SAMPLES: i64 = 1_000_000;
    let mut group = c.benchmark_group("time_series");
    for nodes in [1, 16] {
        let dir = tempfile::tempdir().unwrap();
        let series = series(dir.path(), SAMPLES, nodes);
        let mut i = 0i64;
        group.bench_function(BenchmarkId::new("latest/nodes", nodes), |b| {
            b.iter(|| black_box(series.latest().map(|l| l.timestamp())))
        });
        group.bench_function(BenchmarkId::new("start_timestamp/nodes", nodes), |b| {
            b.iter(|| black_box(series.start_timestamp()))
        });
        group.bench_function(BenchmarkId::new("nearest/nodes", nodes), |b| {
            b.iter(|| {
                i = (i + 7_919) % SAMPLES;
                black_box(
                    series
                        .binary_search_nearest(Timestamp(i), true)
                        .map(|n| n.timestamp()),
                )
            })
        });
    }
    group.finish();
}

/// Sums every value in a window of a million-sample series, as a plot or
/// query scans it.
fn time_series_range(c: &mut Criterion) {
    const SAMPLES: i64 = 1_000_000;
    let dir = tempfile::tempdir().unwrap();
    let series = series(dir.path(), SAMPLES, 16);
    let mut group = c.benchmark_group("time_series/range");
    for window in [1_000, 100_000] {
        let mut start = 0i64;
        group.throughput(Throughput::Elements(window as u64));
        group.bench_function(BenchmarkId::new("samples", window), |b| {
            b.iter(|| {
                start = (start + 104_729) % (SAMPLES - window);
                let slice = series
                    .get_range(Timestamp(start)..Timestamp(start + window))
                    .unwrap();
                let sum: f64 = slice
                    .as_iter()
                    .flat_map(|node| {
                        node.data()
                            .chunks_exact(8)
                            .map(|v| f64::from_le_bytes(v.try_into().unwrap()))
                            .collect::<Vec<_>>()
                    })
                    .sum();
                black_box(sum)
            })
        });
    }
    group.finish();
}

/// One tick of a real-time stream: the newest sample at or before the
/// stream's time from every component, packed into a table, as
/// `DBVisitor::populate_table` does.
fn stream_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream/real_time_tick");
    for components in [64, 1024] {
        let dir = tempfile::tempdir().unwrap();
        let all: Vec<TimeSeries> = (0..components)
            .map(|i| series(&dir.path().join(i.to_string()), 1_000, 1))
            .collect();
        let mut table = Vec::with_capacity(components * 16);
        let mut now = 0i64;
        group.throughput(Throughput::Elements(components as u64));
        group.bench_function(BenchmarkId::new("components", components), |b| {
            b.iter(|| {
                now = (now + 1) % 1_000;
                table.clear();
                for series in &all {
                    let Some(start) = series.start_timestamp() else {
                        continue;
                    };
                    let tick = start.max(Timestamp(now));
                    let Some(nearest) = series.binary_search_nearest(tick, true) else {
                        continue;
                    };
                    table.extend_from_slice(&nearest.timestamp().0.to_le_bytes());
                    table.extend_from_slice(nearest.data());
                }
                black_box(&table);
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    ingest_persisted,
    time_series_write,
    time_series_read,
    time_series_range,
    stream_tick
);
criterion_main!(benches);
