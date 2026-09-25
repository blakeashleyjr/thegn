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
use thegn_svc::calendar::{AccountAdmission, CalendarBackend, CalendarError, backend_from_account};
use thegn_svc::plugin::proc;

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

/// Peak heap growth (over the live size at entry) while `f` runs.
fn peak_growth<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    (out, PEAK.load(Ordering::SeqCst).saturating_sub(base))
}

/// The bound the streaming visitor has to stay under. Comfortably above one
/// line's own buffers and far below what a JSON tree of the same line costs —
/// `a_value_tree_of_the_same_line_would_blow_the_bound` pins both ends.
const PEAK_BOUND: usize = 4 * 1024 * 1024;

/// One `events` line of `n` small objects, as the plugin writes it.
fn events_line(n: usize) -> String {
    let mut line = String::with_capacity(n * 8 + 64);
    line.push_str(r#"{"method":"events","params":{"events":["#);
    for i in 0..n {
        if i > 0 {
            line.push(',');
        }
        line.push_str(r#"{"a":1}"#);
    }
    line.push_str("]}}");
    line
}

const ELEMENTS: usize = 130_000;

#[test]
fn a_one_mib_events_line_is_walked_not_materialized() {
    // ~1 MiB of `{"a":1}` elements in one line, built in the shell (an exec
    // argument cannot carry a megabyte). Not `{}`: an empty serde_json map
    // does not allocate, so empty elements would understate the tree this
    // test exists to rule out.
    let script = format!(
        r#"printf '{{"method":"events","params":{{"events":['
           yes '{{"a":1}},' | head -n {n} | tr -d '\n'
           printf '{{"a":1}}]}}}}\n'"#,
        n = ELEMENTS - 1
    );
    let backend = backend_from_account(
        &CalendarAccount {
            name: "plug".into(),
            provider: CalendarProviderKind::Command,
            command: vec!["sh".into(), "-c".into(), script],
            ..Default::default()
        },
        AccountAdmission::isolated(AdmissionBudget::default()),
    )
    .expect("command account builds a backend");
    let from = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let to = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let (result, peak) = peak_growth(|| rt.block_on(backend.list_events(from, to, "")));

    // The first element has no uid, so the run fails as malformed — after one
    // element, not after a tree of 130 000.
    assert!(
        matches!(result, Err(CalendarError::Parse(_))),
        "{:?}",
        result.err()
    );
    // The line itself is ~1 MiB and is read into a buffer; a `Value` tree of
    // the same line costs tens of MiB.
    assert!(peak < PEAK_BOUND, "peak heap growth {peak} bytes");
}

#[test]
fn a_value_tree_of_the_same_line_would_blow_the_bound() {
    // The guard above is only a guard if the shape it forbids actually
    // exceeds it: decoding the same line the old way (one `serde_json::Value`
    // for the whole message) must fail the same bound by a wide margin.
    let line = events_line(ELEMENTS);
    // Just under the reader's 1 MiB line cap, which is what the plugin path
    // actually accepts in one line.
    assert!(
        (1_000_000..proc::MAX_LINE_BYTES).contains(&line.len()),
        "line is {} bytes",
        line.len()
    );
    let (value, peak) = peak_growth(|| serde_json::from_str::<serde_json::Value>(&line));
    assert!(value.is_ok());
    assert!(
        peak > PEAK_BOUND,
        "a whole-line Value tree peaked at {peak} bytes, so the streaming \
         bound of {PEAK_BOUND} would not catch a regression"
    );
}
