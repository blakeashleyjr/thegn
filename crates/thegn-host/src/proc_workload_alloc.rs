//! Test-binary-only allocation and row-builder instrumentation. Shipping builds
//! retain the default allocator. Const, non-dropping TLS never allocates here;
//! a counter scope covers only its calling thread, not collector/OS allocations.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Default, Debug, serde::Serialize)]
pub(crate) struct Counts {
    pub allocation_calls: u64,
    pub realloc_calls: u64,
    pub deallocation_calls: u64,
    pub requested_bytes: u64,
    pub process_row_builds: u64,
    pub disk_row_builds: u64,
    pub body_builds: u64,
}
thread_local! { static ACTIVE: Cell<Option<Counts>> = const { Cell::new(None) }; }
struct CountingSystem;
#[global_allocator]
static ALLOCATOR: CountingSystem = CountingSystem;

fn record(update: impl FnOnce(&mut Counts)) {
    // Const Cell TLS needs no allocator or destructor. Thread teardown may make
    // it inaccessible, in which case no measurement scope can be active.
    let _active_thread = ACTIVE.try_with(|slot| {
        if let Some(mut value) = slot.get() {
            update(&mut value);
            slot.set(Some(value));
        }
    });
}
// SAFETY: every allocator operation forwards exactly the caller's pointer,
// layout and size to System; instrumentation neither allocates nor accesses
// allocated memory. It has no panicking arithmetic or reentrant callbacks.
unsafe impl GlobalAlloc for CountingSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(|c| {
            c.allocation_calls = c.allocation_calls.saturating_add(1);
            c.requested_bytes = c.requested_bytes.saturating_add(layout.size() as u64);
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(|c| {
            c.allocation_calls = c.allocation_calls.saturating_add(1);
            c.requested_bytes = c.requested_bytes.saturating_add(layout.size() as u64);
        });
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(|c| {
            c.realloc_calls = c.realloc_calls.saturating_add(1);
            c.requested_bytes = c.requested_bytes.saturating_add(size as u64);
        });
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(|c| c.deallocation_calls = c.deallocation_calls.saturating_add(1));
        unsafe { System.dealloc(ptr, layout) }
    }
}
struct Scope {
    prior: Option<Counts>,
}
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|slot| slot.set(self.prior));
    }
}
pub(crate) fn measure<T>(run: impl FnOnce() -> T) -> (T, Counts) {
    let scope = Scope {
        prior: ACTIVE.with(|slot| slot.replace(Some(Counts::default()))),
    };
    let value = run();
    let counts = ACTIVE.with(|slot| slot.get().unwrap_or_default());
    drop(scope);
    (value, counts)
}
pub(crate) fn process_rows() {
    record(|c| c.process_row_builds = c.process_row_builds.saturating_add(1));
}
pub(crate) fn disk_rows() {
    record(|c| c.disk_row_builds = c.disk_row_builds.saturating_add(1));
}
pub(crate) fn body() {
    record(|c| c.body_builds = c.body_builds.saturating_add(1));
}

#[test]
fn scoped_counter_excludes_other_threads_and_restores_after_unwind() {
    let (_result, counts) = measure(|| {
        let data = vec![7u8; std::hint::black_box(4096)];
        std::hint::black_box(&data);
        process_rows();
        disk_rows();
        body();
    });
    assert!(counts.allocation_calls >= 1 && counts.requested_bytes >= 4096);
    assert_eq!(
        (
            counts.process_row_builds,
            counts.disk_row_builds,
            counts.body_builds
        ),
        (1, 1, 1)
    );
    let (release, gate) = std::sync::mpsc::channel();
    let (done, observed) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        gate.recv().unwrap();
        process_rows();
        let bytes = vec![0u8; std::hint::black_box(1024 * 1024)];
        std::hint::black_box(bytes);
        done.send(()).unwrap();
    });
    let (_result, counts) = measure(|| {
        release.send(()).unwrap();
        observed.recv().unwrap();
    });
    worker.join().unwrap();
    assert_eq!(counts.process_row_builds, 0);
    assert!(
        counts.requested_bytes < 1024 * 1024,
        "other thread allocations are excluded"
    );
    let _injected_panic =
        std::panic::catch_unwind(|| measure(|| panic!("injected measurement unwind")));
    assert!(ACTIVE.with(|slot| slot.get()).is_none());
}
