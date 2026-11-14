//! Fixed-iteration adversarial plugin soak.
//!
//! The RSS ceiling is 64 MiB of growth (16,384 resident pages at the usual
//! 4 KiB Linux page size). It is intentionally generous and measures growth
//! from this test's baseline rather than absolute RSS, so machine size and the
//! test runner's ambient allocations do not set the result. `/proc/self/statm`
//! is optional: environments without procfs print a skip reason and retain the
//! queue, output, subscription, and ownership assertions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thegn_core::control_wire::EventFrame;
use thegn_core::plugin_api::RpcMessage;
use thegn_svc::plugin::proc::{MAX_LINE_BYTES, MAX_RUN_BYTES, NdjsonSink, spawn_ndjson_stream};
use thegn_svc::plugin::{SessionEvent, spawn_ndjson};

use super::feed_hub::{Feed, FeedHub, FeedSource, MAX_FEED_SUBSCRIBERS, SubscribeError};
use super::inbox::{
    MAX_QUEUED_EVENTS_PER_PLUGIN, MAX_QUEUED_EVENTS_TOTAL, PluginInbox, PushOutcome,
};

const PLUGIN: &str = "adversarial-soak";
const SOAK_ITERATIONS: usize = 8;
const PRODUCER_MESSAGES: usize = 2_048;
const REQUESTS_PER_ITERATION: usize = 2_048;
const RETAINED_MESSAGES: usize = 2_048;
const RETAINED_PAYLOAD_BYTES: usize = 5_000;
const RSS_GROWTH_CEILING_PAGES: u64 = 16_384;
const CENSUS_CAP: usize = 256;
const PLUGIN_THREADS: &[&str] = &[
    super::feed_hub::FEED_HUB_THREAD,
    "thegn-plug-pipe",
    "thegn-plug-out",
    "thegn-plug-err",
    "thegn-plug-reap",
];

const FAST_PRODUCER: &str = r#"
i=0
while [ "$i" -lt 2048 ]; do
  printf '{"method":"emit","params":{"iteration":%s,"sequence":%s}}\n' "$1" "$i"
  i=$((i + 1))
done
printf '%*s' 1048577 ''
"#;

const RETAINED_OUTPUT_PRODUCER: &str = r#"
payload=$(printf '%05000d' 0)
i=0
while [ "$i" -lt 2048 ]; do
  printf '{"method":"emit","params":{"payload":"%s"}}\n' "$payload"
  i=$((i + 1))
done
printf '%*s' 1048577 ''
"#;

struct InboxSink {
    inbox: Arc<PluginInbox>,
}

impl NdjsonSink for InboxSink {
    fn line(&mut self, text: &str) -> bool {
        if let Ok(message) = serde_json::from_str::<RpcMessage>(text) {
            self.inbox
                .push(PLUGIN.to_owned(), 1, SessionEvent::Message(message));
        }
        true
    }
}

struct FeedDrop(std::sync::mpsc::Sender<()>);

impl Drop for FeedDrop {
    fn drop(&mut self) {
        if let Err(error) = self.0.send(()) {
            tracing::debug!(target: "thegn::plugin", %error, "feed soak drop probe receiver is gone");
        }
    }
}

fn producer_argv(script: &str, iteration: usize) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        script.into(),
        "adversarial-plugin".into(),
        iteration.to_string(),
    ]
}

fn request(id: u64) -> SessionEvent {
    SessionEvent::Message(RpcMessage {
        id: Some(id),
        method: "host.call".into(),
        params: serde_json::json!({"capability":"soak"}),
    })
}

fn rss_pages() -> Option<u64> {
    let statm = match std::fs::read_to_string("/proc/self/statm") {
        Ok(statm) => statm,
        Err(_) => return None,
    };
    let resident = statm.split_whitespace().nth(1)?;
    let Ok(pages) = resident.parse::<u64>() else {
        return None;
    };
    Some(pages)
}

fn child_ids() -> Option<BTreeSet<u32>> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return None;
    };
    let mut ids = BTreeSet::new();
    let mut any_read = false;
    for task in tasks.flatten() {
        let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        any_read = true;
        for child in children.split_whitespace() {
            let Ok(pid) = child.parse::<u32>() else {
                continue;
            };
            ids.insert(pid);
        }
    }
    any_read.then_some(ids)
}

fn named_thread_count(name: &str) -> Option<usize> {
    let mut samples = Vec::new();
    crate::platform::thread_census::sample_self(&mut samples, CENSUS_CAP);
    if samples.is_empty() {
        None
    } else {
        Some(
            samples
                .iter()
                .filter(|sample| sample.comm_str() == name)
                .count(),
        )
    }
}

fn plugin_thread_count() -> Option<usize> {
    let counts = PLUGIN_THREADS
        .iter()
        .map(|name| named_thread_count(name))
        .collect::<Option<Vec<_>>>()?;
    Some(counts.into_iter().sum())
}

fn wait_for_plugin_thread_count_at_most(expected: usize) {
    for _ in 0..2_500 {
        if plugin_thread_count().is_some_and(|count| count <= expected) {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        plugin_thread_count().is_none_or(|count| count <= expected),
        "plugin soak left named worker threads after counted cleanup"
    );
}

fn assert_retained_output_is_bounded() {
    let run = spawn_ndjson(
        &producer_argv(RETAINED_OUTPUT_PRODUCER, 0),
        &BTreeMap::new(),
        None,
        Duration::from_secs(10),
    )
    .expect("retained-output adversarial plugin completes");
    assert!(run.truncated, "retained output must hit an output ceiling");
    assert!(run.outcome.reaped, "retained-output child must be reaped");
    assert!(run.outcome.drained, "retained-output pipes must drain");
    assert!(
        !run.messages.is_empty(),
        "retained-output plugin produced frames"
    );
    assert!(
        run.messages.len() <= RETAINED_MESSAGES,
        "retained messages escaped the documented output budget: {}",
        run.messages.len()
    );
    let retained_line_bytes = RETAINED_PAYLOAD_BYTES.min(MAX_LINE_BYTES);
    assert!(
        run.messages.len() <= MAX_RUN_BYTES / (retained_line_bytes + 32) + 1,
        "retained messages escaped the byte budget: {}",
        run.messages.len()
    );
}

fn feed_source() -> (
    FeedSource,
    tokio::sync::mpsc::Sender<EventFrame>,
    std::sync::mpsc::Receiver<()>,
) {
    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(1);
    let (drop_tx, drop_rx) = std::sync::mpsc::channel();
    let feed = Feed::new(frame_rx, FeedDrop(drop_tx));
    let slot = Arc::new(Mutex::new(Some(feed)));
    let source_slot = Arc::clone(&slot);
    let source: FeedSource = Arc::new(move || {
        let source_slot = Arc::clone(&source_slot);
        Box::pin(async move {
            source_slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        })
    });
    (source, frame_tx, drop_rx)
}

#[test]
fn plugin_soak_is_bounded_across_fixed_adversarial_iterations() {
    let _state = crate::testenv::IsolatedState::new();
    let rss_before = rss_pages();
    let children_before = child_ids();
    let plugin_threads_before = plugin_thread_count();

    assert_retained_output_is_bounded();

    let inbox = Arc::new(PluginInbox::default());
    let mut peak_rss_growth = rss_before
        .and_then(|before| rss_pages().map(|current| current.saturating_sub(before)))
        .unwrap_or(0);
    let mut rejected_requests = 0;
    let mut dropped_events = 0;

    for iteration in 0..SOAK_ITERATIONS {
        let run = spawn_ndjson_stream(
            &producer_argv(FAST_PRODUCER, iteration),
            &BTreeMap::new(),
            None,
            Duration::from_secs(10),
            InboxSink {
                inbox: Arc::clone(&inbox),
            },
        )
        .expect("fast producer completes");
        assert!(run.truncated, "newline-free output must be truncated");
        assert!(run.outcome.reaped, "fast producer child must be reaped");
        assert!(run.outcome.drained, "fast producer pipes must drain");

        for request_id in 0..REQUESTS_PER_ITERATION {
            match inbox.push(
                PLUGIN.to_owned(),
                1,
                request((iteration * REQUESTS_PER_ITERATION + request_id) as u64),
            ) {
                PushOutcome::Rejected => rejected_requests += 1,
                PushOutcome::Dropped => dropped_events += 1,
                other => panic!("request pressure was not bounded: {other:?}"),
            }
        }

        // One counted pop is deliberately much slower than the producer.
        assert!(
            inbox.pop().is_some(),
            "slow consumer should drain one event"
        );
        if let Some(before) = rss_before
            && let Some(current) = rss_pages()
        {
            peak_rss_growth = peak_rss_growth.max(current.saturating_sub(before));
        }
    }

    assert!(rejected_requests > 0, "request admission must be rejected");
    assert!(dropped_events > 0, "request admission must eventually drop");
    assert!(
        inbox.len() <= MAX_QUEUED_EVENTS_PER_PLUGIN.min(MAX_QUEUED_EVENTS_TOTAL),
        "dispatcher/event queue depth escaped its cap: {}",
        inbox.len()
    );

    let diagnostics = inbox.take_diagnostics();
    let counts = diagnostics
        .get(PLUGIN)
        .copied()
        .expect("adversarial plugin has pressure diagnostics");
    assert!(counts.dropped > 0, "dropped event counter must be non-zero");
    assert!(
        counts.rejected > 0,
        "RateLimited/rejected counter must be non-zero"
    );
    assert!(
        counts.dropped <= SOAK_ITERATIONS * (PRODUCER_MESSAGES + REQUESTS_PER_ITERATION),
        "dropped counter exceeded the fixed producer budget: {}",
        counts.dropped
    );
    assert!(
        counts.rejected <= MAX_QUEUED_EVENTS_PER_PLUGIN,
        "retained rejection queue escaped its cap: {}",
        counts.rejected
    );
    let status_rows = {
        let mut totals = crate::plugin_stats::Totals::default();
        totals.add_inbox(PLUGIN, counts.coalesced, counts.dropped, counts.rejected);
        totals.rows(&BTreeMap::new())
    };
    let status_row = status_rows
        .iter()
        .find(|row| row.plugin == PLUGIN)
        .expect("queue pressure reaches the status counter projection");
    assert_eq!(
        status_row.queue_drops,
        (counts.dropped + counts.rejected) as u64,
        "status counter must retain dropped and RateLimited queue pressure"
    );

    let mut retained_rejections = 0;
    for _ in 0..MAX_QUEUED_EVENTS_TOTAL {
        if inbox.pop_rejected().is_none() {
            break;
        }
        retained_rejections += 1;
    }
    assert!(retained_rejections <= MAX_QUEUED_EVENTS_TOTAL);
    for _ in 0..MAX_QUEUED_EVENTS_TOTAL {
        if inbox.pop().is_none() {
            break;
        }
    }
    assert!(
        inbox.is_empty(),
        "bounded queues must be drainable after soak"
    );

    let (source, frame_tx, feed_dropped) = feed_source();
    let mut hub = FeedHub::new(source);
    let seen = Arc::new(AtomicUsize::new(0));
    let mut subscriptions = Vec::with_capacity(MAX_FEED_SUBSCRIBERS);
    for _ in 0..MAX_FEED_SUBSCRIBERS {
        let seen = Arc::clone(&seen);
        subscriptions.push(
            hub.subscribe(Box::new(move |_| {
                seen.fetch_add(1, Ordering::Relaxed);
                true
            }))
            .expect("subscription within documented cap"),
        );
    }
    assert!(matches!(
        hub.subscribe(Box::new(|_| true)),
        Err(SubscribeError::Full)
    ));
    for _ in 0..SOAK_ITERATIONS * 4 {
        frame_tx
            .blocking_send(EventFrame::Activity { json: "{}".into() })
            .expect("feed producer remains connected during soak");
    }
    drop(frame_tx);
    feed_dropped
        .recv_timeout(Duration::from_secs(10))
        .expect("feed owner releases its retained stream");
    drop(subscriptions);
    assert!(
        seen.load(Ordering::Relaxed) > 0,
        "subscribers consumed feed events"
    );
    if let Some(before) = plugin_threads_before {
        wait_for_plugin_thread_count_at_most(before);
    } else {
        eprintln!("plugin soak: skipping thread census; platform helper returned no samples");
    }

    if let Some(before) = rss_before {
        let after = rss_pages().expect("RSS remained readable after the soak");
        peak_rss_growth = peak_rss_growth.max(after.saturating_sub(before));
        assert!(
            peak_rss_growth <= RSS_GROWTH_CEILING_PAGES,
            "peak RSS growth exceeded the generous 64 MiB ceiling: {peak_rss_growth} pages"
        );
    } else {
        eprintln!("plugin soak: skipping RSS ceiling; /proc/self/statm is unavailable");
    }

    if let Some(before) = children_before {
        let after = child_ids().expect("child census remained readable after the soak");
        assert!(
            after.is_subset(&before),
            "plugin soak left child processes behind: before={before:?}, after={after:?}"
        );
    } else {
        eprintln!("plugin soak: skipping child census; /proc task children is unavailable");
    }
}
