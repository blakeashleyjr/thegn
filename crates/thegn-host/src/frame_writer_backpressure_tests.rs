//! Backpressure scenarios for the terminal writer (THE-667): a real writer
//! worker drives a gated sink that blocks inside every write until the test
//! releases it. Every step waits on a logical barrier (the sink announcing the
//! write it entered, the completion condvar, the wake channel); the only
//! durations are failure bounds that a passing run never reaches.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    FRAME_QUEUE_DEPTH, FrameWriter, Msg, OOB_QUEUE_BYTES, OOB_QUEUE_COUNT, OobKind, PARSER_RESET,
    WriterStatus,
};
use crate::perf_timing::{FrameStamp, WriterMetrics};

/// Failure bound only: a passing run never waits this long.
const FAILURE_BOUND: Duration = Duration::from_secs(5);

/// The test's side of a sink that blocks in every write until released.
struct GatedSink {
    /// Each sink call announces the bytes it entered with, then blocks.
    started: Receiver<Vec<u8>>,
    /// One result per sink call; sending it returns that call.
    release: Sender<io::Result<()>>,
    /// Bytes of every sink call that returned `Ok`, in call order.
    written: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Every loop wake the writer requested.
    wakes: Arc<AtomicUsize>,
    woken: Receiver<&'static str>,
}

impl GatedSink {
    fn next_started(&self) -> Vec<u8> {
        self.started
            .recv_timeout(FAILURE_BOUND)
            .expect("writer worker never entered the sink")
    }

    fn release_ok(&self) {
        self.release.send(Ok(())).expect("sink is gone");
    }

    fn release_err(&self, kind: io::ErrorKind) {
        self.release.send(Err(kind.into())).expect("sink is gone");
    }

    fn written(&self) -> Vec<Vec<u8>> {
        self.written.lock().expect("sink log").clone()
    }

    fn next_wake(&self) -> &'static str {
        self.woken
            .recv_timeout(FAILURE_BOUND)
            .expect("writer never woke the loop")
    }
}

/// An async-mode writer (worker-managed frames, as in production) over a gated
/// sink.
fn gated_writer() -> (FrameWriter, GatedSink) {
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<io::Result<()>>();
    let (woken_tx, woken) = mpsc::channel();
    let woken_tx = Mutex::new(woken_tx);
    let written = Arc::new(Mutex::new(Vec::new()));
    let wakes = Arc::new(AtomicUsize::new(0));
    let sink_written = Arc::clone(&written);
    let wake_count = Arc::clone(&wakes);
    let writer = FrameWriter::spawn_with(
        false,
        true,
        move |reason| {
            wake_count.fetch_add(1, Ordering::SeqCst);
            if let Err(error) = woken_tx.lock().expect("wake log").send(reason) {
                tracing::trace!(?error, "test finished before the writer wake");
            }
        },
        move |bytes| {
            if started_tx.send(bytes.to_vec()).is_err() {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let result = release_rx
                .recv()
                .unwrap_or_else(|_| Err(io::ErrorKind::BrokenPipe.into()));
            if result.is_ok() {
                sink_written.lock().expect("sink log").push(bytes.to_vec());
            }
            result
        },
    );
    (
        writer,
        GatedSink {
            started,
            release,
            written,
            wakes,
            woken,
        },
    )
}

fn frame(i: usize) -> Vec<u8> {
    format!("frame-{i}").into_bytes()
}

/// Barrier: block until the worker has finished accounting for `target`
/// messages (frames, OOB and clipboard all advance `completed_seq`).
fn wait_completed(writer: &FrameWriter, target: u64) {
    let deadline = Instant::now() + FAILURE_BOUND;
    let mut q = writer.inner.q.lock().expect("writer queue");
    while q.completed_seq < target {
        let now = Instant::now();
        assert!(
            now < deadline,
            "writer completed {} of {target} messages",
            q.completed_seq
        );
        let (next, _) = writer
            .inner
            .done_cv
            .wait_timeout(q, deadline - now)
            .expect("writer queue");
        q = next;
    }
}

/// Read the metrics ledger with a blocking lock, for points where the worker
/// may briefly hold the queue lock (`take_metrics` deliberately never waits).
fn drain_metrics(writer: &FrameWriter) -> WriterMetrics {
    std::mem::take(&mut writer.inner.q.lock().expect("writer queue").metrics)
}

#[test]
fn blocked_sink_retains_at_most_the_frame_depth_and_refuses_without_queueing() {
    let (writer, sink) = gated_writer();
    assert!(writer.submit_frame(frame(0), None));
    assert_eq!(sink.next_started(), frame(0));

    // The in-flight frame was popped; the queue admits exactly the depth.
    for i in 1..=FRAME_QUEUE_DEPTH {
        assert!(writer.frame_slot_free(), "slot {i} should be free");
        assert!(writer.submit_frame(frame(i), None));
    }
    assert!(!writer.frame_slot_free());
    let refused = frame(99);
    assert!(!writer.submit_frame(refused.clone(), None));
    assert!(!writer.submit_frame(refused, None));
    {
        let q = writer.inner.q.lock().expect("writer queue");
        assert_eq!(q.frames_queued, FRAME_QUEUE_DEPTH);
        let queued = q
            .msgs
            .iter()
            .filter_map(|msg| match msg {
                Msg::Frame(bytes, ..) => Some(bytes.clone()),
                Msg::Oob(..) | Msg::Clipboard(..) => None,
            })
            .collect::<Vec<_>>();
        let expected = (1..=FRAME_QUEUE_DEPTH).map(frame).collect::<Vec<_>>();
        assert_eq!(queued, expected, "refused frames must not be queued");
        assert_eq!(q.msgs.len(), FRAME_QUEUE_DEPTH);
        assert_eq!(q.submitted_seq, 1 + FRAME_QUEUE_DEPTH as u64);
        assert_eq!(q.completed_seq, 0);
    }
    assert_eq!(writer.take_status(), WriterStatus::Ok);

    // Releasing one write frees one slot only once the worker pops the next.
    sink.release_ok();
    assert_eq!(sink.next_started(), frame(1));
    assert!(writer.frame_slot_free());
    let late = FRAME_QUEUE_DEPTH + 1;
    assert!(writer.submit_frame(frame(late), None));
    assert!(!writer.frame_slot_free());

    for _ in 1..=late {
        sink.release_ok();
    }
    wait_completed(&writer, 1 + late as u64);
    assert_eq!(sink.written(), (0..=late).map(frame).collect::<Vec<_>>());
    assert!(writer.frame_slot_free());
    assert_eq!(writer.take_status(), WriterStatus::Ok);
    drop(writer);
}

#[test]
fn oob_count_cap_refuses_with_bytes_returned_and_drains_in_order() {
    let (writer, sink) = gated_writer();
    assert!(writer.submit_frame(frame(0), None));
    assert_eq!(sink.next_started(), frame(0));

    let payloads = (0..OOB_QUEUE_COUNT)
        .map(|i| format!("oob-{i}").into_bytes())
        .collect::<Vec<_>>();
    for payload in &payloads {
        assert_eq!(
            writer.try_submit_oob(OobKind::Other, payload.clone()),
            Ok(())
        );
    }
    let refused = b"oob-refused".to_vec();
    assert_eq!(
        writer.try_submit_oob(OobKind::Graphics, refused.clone()),
        Err(refused.clone())
    );
    {
        let q = writer.inner.q.lock().expect("writer queue");
        assert_eq!(q.oob_in_flight, OOB_QUEUE_COUNT);
        assert_eq!(q.msgs.len(), OOB_QUEUE_COUNT, "the refusal was queued");
        assert_eq!(q.oob_bytes, payloads.iter().map(Vec::len).sum::<usize>());
    }
    assert_eq!(
        writer.take_status(),
        WriterStatus::Ok,
        "refusal is not fatal"
    );

    for _ in 0..=OOB_QUEUE_COUNT {
        sink.release_ok();
    }
    wait_completed(&writer, 1 + OOB_QUEUE_COUNT as u64);
    let mut expected = vec![frame(0)];
    expected.extend(payloads.iter().cloned());
    assert_eq!(
        sink.written(),
        expected,
        "queued OOB must drain in FIFO order"
    );
    {
        let q = writer.inner.q.lock().expect("writer queue");
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
    }

    // The retained bytes are admitted once capacity is back.
    assert_eq!(
        writer.try_submit_oob(OobKind::Graphics, refused.clone()),
        Ok(())
    );
    sink.release_ok();
    wait_completed(&writer, 2 + OOB_QUEUE_COUNT as u64);
    assert_eq!(sink.written().last(), Some(&refused));
    drop(writer);
}

#[test]
fn oob_byte_cap_refuses_past_four_mib_with_bytes_returned() {
    let (writer, sink) = gated_writer();
    assert!(writer.submit_frame(frame(0), None));
    assert_eq!(sink.next_started(), frame(0));

    let big = vec![0xA5; OOB_QUEUE_BYTES - 1];
    assert_eq!(
        writer.try_submit_oob(OobKind::Graphics, big.clone()),
        Ok(())
    );
    let two = vec![1, 2];
    assert_eq!(
        writer.try_submit_oob(OobKind::Other, two.clone()),
        Err(two.clone())
    );
    let one = vec![7];
    assert_eq!(writer.try_submit_oob(OobKind::Other, one.clone()), Ok(()));
    {
        let q = writer.inner.q.lock().expect("writer queue");
        assert_eq!(q.oob_bytes, OOB_QUEUE_BYTES);
        assert_eq!(q.oob_in_flight, 2);
        assert_eq!(q.msgs.len(), 2);
    }
    let full = vec![8];
    assert_eq!(
        writer.try_submit_oob(OobKind::Other, full.clone()),
        Err(full)
    );
    assert_eq!(writer.take_status(), WriterStatus::Ok);

    for _ in 0..3 {
        sink.release_ok();
    }
    wait_completed(&writer, 3);
    let written = sink.written();
    assert!(
        written == vec![frame(0), big, one],
        "unexpected write order: lengths {:?}",
        written.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert_eq!(writer.inner.q.lock().expect("writer queue").oob_bytes, 0);
    assert_eq!(writer.try_submit_oob(OobKind::Other, two.clone()), Ok(()));
    sink.release_ok();
    wait_completed(&writer, 4);
    assert_eq!(sink.written().last(), Some(&two));
    drop(writer);
}

#[test]
fn delayed_frame_completes_only_after_sink_returns_and_failed_frame_never_counts() {
    let (writer, sink) = gated_writer();
    let stamp = FrameStamp {
        queued_at: Instant::now(),
        input_at: Some(Instant::now()),
    };
    assert!(writer.submit_frame(frame(0), Some(stamp)));
    assert_eq!(sink.next_started(), frame(0));
    assert!(writer.submit_frame(frame(1), Some(stamp)));

    // Blocked inside the sink: nothing is complete, and the ledger is readable
    // without waiting on the writer.
    let blocked = writer
        .take_metrics()
        .expect("a blocked sink must not hold the queue lock");
    assert_eq!(blocked.completion_us.count(), 0);
    assert_eq!(blocked.input_completion_us.count(), 0);
    assert_eq!(blocked.failed_frames, 0);
    assert_eq!(
        writer.inner.q.lock().expect("writer queue").completed_seq,
        0
    );

    sink.release_ok();
    wait_completed(&writer, 1);
    // The worker is now blocked in frame 1's write, so the lock is free.
    assert_eq!(sink.next_started(), frame(1));
    let first = writer
        .take_metrics()
        .expect("a blocked sink must not hold the queue lock");
    assert_eq!(first.completion_us.count(), 1);
    assert_eq!(first.input_completion_us.count(), 1);
    assert_eq!(first.failed_frames, 0);
    assert_eq!(
        writer.inner.q.lock().expect("writer queue").completed_seq,
        1
    );

    // A delayed failure is accounted only as a failure.
    sink.release_err(io::ErrorKind::Interrupted);
    assert_eq!(sink.next_wake(), "writer state changed");
    wait_completed(&writer, 2);
    let failed = drain_metrics(&writer);
    assert_eq!(failed.failed_frames, 1);
    assert_eq!(failed.completion_us.count(), 0);
    assert_eq!(failed.input_completion_us.count(), 0);
    assert_eq!(
        writer.take_status(),
        WriterStatus::Transient(OobKind::Other)
    );
    assert_eq!(writer.take_status(), WriterStatus::Ok);

    // The resync frame starts with the parser reset and completes normally.
    assert!(writer.submit_frame(frame(2), Some(stamp)));
    assert_eq!(sink.next_started(), PARSER_RESET);
    let resetting = writer
        .take_metrics()
        .expect("a blocked sink must not hold the queue lock");
    assert_eq!(resetting.completion_us.count(), 0);
    assert_eq!(resetting.failed_frames, 0);
    sink.release_ok();
    assert_eq!(sink.next_started(), frame(2));
    sink.release_ok();
    wait_completed(&writer, 3);
    let resynced = drain_metrics(&writer);
    assert_eq!(resynced.completion_us.count(), 1);
    assert_eq!(resynced.failed_frames, 0);
    assert_eq!(writer.take_status(), WriterStatus::Ok);
    assert_eq!(
        sink.written(),
        vec![frame(0), PARSER_RESET.to_vec(), frame(2)],
        "the failed frame's bytes never count as written"
    );
    drop(writer);
}

#[test]
fn releasing_the_sink_after_pressure_restores_ok_without_losing_the_wake() {
    let (writer, sink) = gated_writer();
    assert!(writer.submit_frame(frame(0), None));
    assert_eq!(sink.next_started(), frame(0));
    for i in 1..=FRAME_QUEUE_DEPTH {
        assert!(writer.submit_frame(frame(i), None));
    }
    for i in 0..OOB_QUEUE_COUNT {
        assert_eq!(writer.try_submit_oob(OobKind::Other, vec![i as u8]), Ok(()));
    }
    // Pressure on both queues; a retained OOB refusal arms the capacity wake.
    assert!(!writer.frame_slot_free());
    assert!(!writer.submit_frame(frame(99), None));
    let retained = b"retained".to_vec();
    assert_eq!(
        writer.try_submit_oob(OobKind::Graphics, retained.clone()),
        Err(retained.clone())
    );
    assert_eq!(writer.take_status(), WriterStatus::Ok);
    assert_eq!(
        sink.wakes.load(Ordering::SeqCst),
        0,
        "pressure alone must not wake the loop"
    );

    let total = 1 + FRAME_QUEUE_DEPTH + OOB_QUEUE_COUNT;
    for _ in 0..total {
        sink.release_ok();
    }
    assert_eq!(sink.next_wake(), "writer state changed");
    wait_completed(&writer, total as u64);
    assert!(sink.wakes.load(Ordering::SeqCst) >= 1);
    assert_eq!(writer.take_status(), WriterStatus::Ok);
    assert!(writer.frame_slot_free());
    {
        let q = writer.inner.q.lock().expect("writer queue");
        assert_eq!(q.frames_queued, 0);
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
        assert!(q.msgs.is_empty());
    }
    assert_eq!(sink.written().len(), total);
    assert_eq!(writer.try_submit_oob(OobKind::Graphics, retained), Ok(()));
    sink.release_ok();
    wait_completed(&writer, total as u64 + 1);
    drop(writer);
}

#[test]
fn shutdown_with_a_still_blocked_sink_returns_within_the_bound() {
    let (writer, sink) = gated_writer();
    assert!(writer.submit_frame(frame(0), None));
    assert_eq!(sink.next_started(), frame(0));
    for i in 1..=FRAME_QUEUE_DEPTH {
        assert!(writer.submit_frame(frame(i), None));
    }
    assert_eq!(
        writer.try_submit_oob(OobKind::Other, b"oob".to_vec()),
        Ok(())
    );
    let inner = Arc::clone(&writer.inner);

    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(writer);
        dropped_tx
            .send(())
            .expect("test stopped waiting for the drop");
    });
    dropped_rx
        .recv_timeout(FAILURE_BOUND)
        .expect("dropping the writer waited on the blocked sink instead of detaching");
    dropper.join().expect("dropper thread panicked");
    {
        let q = inner.q.lock().expect("writer queue");
        assert!(matches!(q.status, WriterStatus::Fatal(_)));
        assert!(q.msgs.is_empty(), "queued work must be abandoned");
        assert_eq!(q.frames_queued, 0);
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
        assert!(!q.worker_done, "the worker is still inside the sink");
    }

    // Releasing the detached worker writes nothing that was queued.
    sink.release_ok();
    let deadline = Instant::now() + FAILURE_BOUND;
    let mut q = inner.q.lock().expect("writer queue");
    while !q.worker_done {
        let now = Instant::now();
        assert!(now < deadline, "detached writer worker never exited");
        let (next, _) = inner
            .cv
            .wait_timeout(q, deadline - now)
            .expect("writer queue");
        q = next;
    }
    drop(q);
    assert_eq!(sink.written(), vec![frame(0)]);
}
