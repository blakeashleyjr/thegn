//! Allocation regression check for warmed SeriesStore operations.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use thegn_core::series_store::{Class, SeriesBuf, SeriesKey, SeriesStore, Tier};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
/// The allocator counter is process-wide, so this test must run alone.
static SERIAL: Mutex<()> = Mutex::new(());
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn warmed_push_and_read_do_not_allocate() {
    let _guard = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let mut store = SeriesStore::new();
    let mut ids = Vec::new();
    for n in 0..20 {
        ids.push(
            store
                .intern(
                    SeriesKey {
                        family: 1,
                        metric: n,
                        instance: 0,
                    },
                    Class::A,
                )
                .unwrap(),
        );
    }
    let mut out = SeriesBuf {
        rows: Vec::with_capacity(1_440),
    };
    for tick in 0_u64..86_500 {
        for (n, id) in ids.iter().enumerate() {
            if !tick.is_multiple_of(97) {
                assert!(store.push(*id, tick * 1_000, n as f32));
            }
        }
    }
    let before = ALLOCS.load(Ordering::SeqCst);
    for tick in 86_500_u64..96_500 {
        for (n, id) in ids.iter().enumerate() {
            if !tick.is_multiple_of(97) {
                assert!(store.push(*id, tick * 1_000, n as f32));
            }
        }
    }
    for id in &ids {
        assert!(store.read(*id, Tier::T0, 0, 96_500_000, &mut out));
        assert_eq!(out.rows.len(), 600);
        assert!(store.read(*id, Tier::T1, 0, 96_500_000, &mut out));
        assert_eq!(out.rows.len(), 720);
        assert!(store.read(*id, Tier::T2, 0, 96_500_000, &mut out));
        assert_eq!(out.rows.len(), 1_440);
    }
    assert_eq!(ALLOCS.load(Ordering::SeqCst), before);
}
