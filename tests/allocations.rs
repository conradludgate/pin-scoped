#![cfg(not(any(miri, pin_scoped_loom)))]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{mpsc, Arc},
};

use pin_scoped::{Scope, ScopeState};

#[derive(Clone, Copy, Debug, Default)]
struct Allocations {
    count: usize,
    bytes: usize,
}

thread_local! {
    static ALLOCATIONS: Cell<Option<Allocations>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn record_allocation(bytes: usize) {
    // Count only the calling thread so Tokio worker activity cannot affect the
    // measurement. TLS access and Cell updates do not allocate or call user code.
    let _ = ALLOCATIONS.try_with(|counter| {
        if let Some(mut allocations) = counter.get() {
            allocations.count = allocations.count.saturating_add(1);
            allocations.bytes = allocations.bytes.saturating_add(bytes);
            counter.set(Some(allocations));
        }
    });
}

// SAFETY: each operation forwards the caller's unchanged pointer, layout and
// size to System, preserving its allocation/deallocation contracts. Recording
// uses only thread-local Cell operations and never inspects allocated storage.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: the GlobalAlloc caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: the GlobalAlloc caller supplies a valid allocation layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: all allocations use System, and the caller supplies the same
        // live allocation pointer and matching layout required by GlobalAlloc.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation(size);
        // SAFETY: the caller supplies a live System allocation, its matching
        // layout, and a new size satisfying GlobalAlloc::realloc's requirements.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measure(f: impl FnOnce()) -> Allocations {
    ALLOCATIONS.with(|counter| counter.set(Some(Allocations::default())));
    f();
    ALLOCATIONS.with(|counter| counter.take().unwrap())
}

#[test]
fn tokio_cancellation_does_not_allocate() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let scope = Box::pin(Scope::with_runtime((), runtime.handle().clone()));
    let (started, ready) = mpsc::channel();
    let started = Arc::new(started);

    let spawn = measure(|| {
        for _ in 0..64 {
            let started = started.clone();
            scope
                .as_ref()
                .spawn(async move |state: ScopeState<'_, ()>| {
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                    std::hint::black_box(state);
                });
        }
    });
    for _ in 0..64 {
        ready.recv().unwrap();
    }

    // Drop outside a runtime context: the measurement covers the scope's
    // cancellation work without Tokio's optional worker-replacement setup.
    let cancel = measure(|| drop(scope));
    eprintln!("64 pending tasks: spawn={spawn:?}, cancel={cancel:?}");
    assert_eq!(cancel.count, 0, "cancellation allocated {cancel:?}");
}
