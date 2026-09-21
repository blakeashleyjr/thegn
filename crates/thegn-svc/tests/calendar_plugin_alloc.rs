//! Allocation-level proof that a plugin's `events` line is admitted without
//! being decoded into an intermediate JSON tree.
//!
//! A counting global allocator records the live-heap high-water mark across
//! one plugin fetch. A 1 MiB line of ~130 000 tiny elements costs tens of MiB
//! as a `serde_json::Value` tree, which is exactly what the streaming visitor
//! in `calendar::command` exists to avoid; this test fails if it comes back.
//! Its own binary, so the allocator affects nothing else.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use thegn_core::calendar::AdmissionBudget;
use thegn_core::config_calendar::{CalendarAccount, CalendarProviderKind};
use thegn_svc::calendar::{AccountAdmission, CalendarBackend, CalendarError, command};

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

#[test]
fn a_one_mib_events_line_is_walked_not_materialized() {
    // ~130 000 `{}` elements in one line, built in the shell (an exec argument
    // cannot carry a megabyte).
    let script = r#"printf '{"method":"events","params":{"events":['
        yes '{},' | head -n 130000 | tr -d '\n'
        printf '{}]}}\n'"#;
    let backend = command::CommandBackend::new(
        &CalendarAccount {
            name: "plug".into(),
            provider: CalendarProviderKind::Command,
            command: vec!["sh".into(), "-c".into(), script.into()],
            ..Default::default()
        },
        AccountAdmission::isolated(AdmissionBudget::default()),
    );
    let from = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let to = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let result = rt.block_on(backend.list_events(from, to, ""));
    let peak = PEAK.load(Ordering::SeqCst).saturating_sub(base);

    // The first element has no uid, so the run fails as malformed — after one
    // element, not after a tree of 130 000.
    assert!(
        matches!(result, Err(CalendarError::Parse(_))),
        "{:?}",
        result.err()
    );
    // The line itself is ~1 MiB and is read into a buffer; a `Value` tree of
    // the same line costs tens of MiB.
    assert!(peak < 8 * 1024 * 1024, "peak heap growth {peak} bytes");
}
