//! Allocation-level proof that ICS admission happens *before* allocation.
//!
//! A counting global allocator records the live-heap high-water mark while the
//! parser runs, so these tests would fail if the parser materialized the
//! document (the old whole-input `unfold`), built every event before checking
//! the budget, or unfolded an oversized line it was going to skip. Its own
//! binary, so the allocator affects nothing else.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use thegn_core::calendar::admission::{AdmissionLimit, MAX_LINE_BYTES};
use thegn_core::calendar::{AdmissionBudget, AdmissionMeter, parse_ics_admitted};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The tests share one allocator, so they must not overlap.
static SERIAL: Mutex<()> = Mutex::new(());

/// Peak heap growth (over the starting live size) while `f` runs.
fn peak_growth<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    (out, PEAK.load(Ordering::SeqCst).saturating_sub(base))
}

fn feed(n: usize) -> String {
    let mut s = String::from("BEGIN:VCALENDAR\r\n");
    for i in 0..n {
        s.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:e{i}\r\nSUMMARY:Event number {i}\r\nDTSTART:20260821T090000Z\r\nEND:VEVENT\r\n"
        ));
    }
    s.push_str("END:VCALENDAR\r\n");
    s
}

#[test]
fn a_huge_feed_allocates_only_up_to_the_budget() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // ~6 MiB of input, 100 000 events; budget 100.
    let doc = feed(100_000);
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::new(100).unwrap());
    let mut out = Vec::new();
    let (r, peak) = peak_growth(|| parse_ics_admitted(&doc, "UTC", &mut meter, &mut out));
    assert_eq!(r.unwrap_err().limit, AdmissionLimit::AccountRecords);
    assert_eq!(out.len(), 100);
    // 100 events plus per-line scratch: far below the input size, and far
    // below what 100 000 events (or an unfolded copy of the input) would cost.
    assert!(peak < 512 * 1024, "peak heap growth {peak} bytes");
}

#[test]
fn an_oversized_skipped_line_is_never_unfolded() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // An 8 MiB folded ATTACH (not a property we keep) inside a small event.
    let mut attach = String::from("ATTACH;ENCODING=BASE64:");
    while attach.len() < 8 * MAX_LINE_BYTES {
        attach.push_str(&"Q".repeat(72));
        attach.push_str("\r\n ");
    }
    let doc =
        format!("BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20260821T090000Z\r\n{attach}\r\nEND:VEVENT\r\n");
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::new(10).unwrap());
    let mut out = Vec::new();
    let (r, peak) = peak_growth(|| parse_ics_admitted(&doc, "UTC", &mut meter, &mut out));
    r.unwrap();
    assert_eq!(out.len(), 1);
    assert!(peak < 64 * 1024, "peak heap growth {peak} bytes");
}
