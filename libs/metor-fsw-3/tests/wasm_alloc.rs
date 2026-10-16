//! Checks that a wasm module's systems do not allocate on the host once they
//! are built.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Ping, open_wasm, panicked};
use metor_fsw_3::coordinator::{Coordinator, CoordinatorConfig};
use metor_fsw_3::{InputConfig, Output, Pack, SystemConfig, SystemTable, Timestamp, system};

/// A global allocator that counts allocations made while the current thread
/// has counting turned on.
struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn count() {
    if ARMED.try_with(Cell::get).unwrap_or(false) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to `System` with the caller's arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller's contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Returns how many allocations this thread makes while `f` runs.
fn allocations(f: impl FnOnce()) -> usize {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ARMED.set(true);
    f();
    ARMED.set(false);
    ALLOCATIONS.load(Ordering::Relaxed)
}

#[derive(Default)]
struct Source(u32);

#[system]
fn source(#[state] s: &mut Source, output: &mut Output<Ping>) {
    s.0 += 1;
    let _ = output.write(&Ping { n: s.0 });
}

/// Builds `source -> middle (echo.<ty>)`, with nothing reading `middle`.
fn built(ty: &str) -> Coordinator {
    let pack = open_wasm();
    let mut table = SystemTable::new();
    table
        .register("source", source(), Source::default)
        .expect("valid records");
    table
        .register_pack("echo", Arc::new(Pack::Wasm(pack)))
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("source", "source"),
            SystemConfig {
                inputs: vec![InputConfig::new("input", "source", "output")],
                ..SystemConfig::new("middle", format!("echo.{ty}"))
            },
        ],
        ..Default::default()
    };
    config.build(&table).expect("the config builds")
}

fn run(coordinator: &mut Coordinator, cycles: i64) {
    for cycle in 1..=cycles {
        coordinator.step(Timestamp(cycle));
    }
}

#[test]
fn test_cyclic_steps_allocate_nothing() {
    let mut coordinator = built("echo");
    assert_eq!(allocations(|| run(&mut coordinator, 64)), 0);
    assert_eq!(panicked(&coordinator).len(), 0);
}

#[test]
fn test_sequence_steps_allocate_nothing() {
    let mut coordinator = built("count_up");
    assert_eq!(allocations(|| run(&mut coordinator, 8)), 0);
    assert_eq!(panicked(&coordinator).len(), 0);
}

#[test]
fn test_counter_counts() {
    let counted = allocations(|| drop(std::hint::black_box(Vec::<u8>::with_capacity(1))));
    assert_eq!(counted, 1);
}
