//! One shared owner for resident plugin subscriptions to the daemon event feed.

use std::any::Any;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thegn_core::config::Config;
use thegn_core::control_wire::EventFrame;
use tokio::sync::mpsc;

pub(crate) const MAX_FEED_SUBSCRIBERS: usize = 8;
pub(crate) const FEED_HUB_THREAD: &str = "thegn-feedhub";

/// The value is control-feed JSON; false means this subscriber is gone.
pub(crate) type FeedSink = Box<dyn FnMut(&serde_json::Value) -> bool + Send>;
pub(crate) struct Feed {
    pub frames: mpsc::Receiver<EventFrame>,
    _keep: Box<dyn Any + Send>,
}

impl Feed {
    pub(crate) fn new(frames: mpsc::Receiver<EventFrame>, keep: impl Any + Send + 'static) -> Self {
        Self {
            frames,
            _keep: Box::new(keep),
        }
    }
}
pub(crate) type FeedFuture = Pin<Box<dyn Future<Output = Option<Feed>> + Send>>;
pub(crate) type FeedSource = Arc<dyn Fn() -> FeedFuture + Send + Sync>;

pub(crate) fn daemon_source(cfg: Arc<Config>) -> FeedSource {
    let cfg = Arc::downgrade(&cfg);
    Arc::new(move || {
        let Some(cfg) = cfg.upgrade() else {
            let future: FeedFuture = Box::pin(async { None });
            return future;
        };
        let future: FeedFuture = Box::pin(async move {
            let client = match crate::cmd::session::connect(&cfg).await {
                Ok(client) => client,
                Err(error) => {
                    tracing::debug!(target: "thegn::plugin", %error, "feed hub: no daemon");
                    return None;
                }
            };
            let stream = match client.subscribe_events().await {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::debug!(target: "thegn::plugin", %error, "feed hub subscribe failed");
                    return None;
                }
            };
            let thegn_svc::control::client::AttachStream {
                frames,
                control,
                end,
            } = stream;
            Some(Feed::new(frames, (client, control, end)))
        });
        future
    })
}

#[derive(Debug)]
pub(crate) enum SubscribeError {
    Full,
    Spawn(String),
}

struct Shared {
    alive: bool,
    tx: Option<mpsc::UnboundedSender<Cmd>>,
    subscribers: usize,
    active: BTreeMap<u64, Arc<AtomicBool>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

enum Cmd {
    Add {
        id: u64,
        sink: FeedSink,
        active: Arc<AtomicBool>,
    },
    Remove(u64),
}

pub(crate) struct FeedHub {
    source: FeedSource,
    shared: Arc<Mutex<Shared>>,
    next_id: u64,
}

impl FeedHub {
    pub(crate) fn new(source: FeedSource) -> Self {
        Self {
            source,
            shared: Arc::new(Mutex::new(Shared {
                alive: false,
                tx: None,
                subscribers: 0,
                active: BTreeMap::new(),
                handle: None,
            })),
            next_id: 1,
        }
    }

    pub(crate) fn subscribe(&mut self, sink: FeedSink) -> Result<FeedSubscription, SubscribeError> {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared.subscribers >= MAX_FEED_SUBSCRIBERS {
            return Err(SubscribeError::Full);
        }
        if shared.alive
            && shared
                .handle
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
        {
            deactivate_all(&mut shared);
        }
        if !shared.alive {
            let (tx, rx) = mpsc::unbounded_channel();
            let worker_shared = Arc::clone(&self.shared);
            let source = Arc::clone(&self.source);
            let handle = thegn_core::thread_name::try_spawn_named(FEED_HUB_THREAD, move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(target: "thegn::plugin", %error, "feed hub runtime failed");
                        finish_hub(&worker_shared);
                        return;
                    }
                };
                runtime.block_on(run(source, rx, worker_shared));
            })
            .map_err(|error| SubscribeError::Spawn(error.to_string()))?;
            shared.alive = true;
            shared.tx = Some(tx);
            shared.handle = Some(handle);
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let active = Arc::new(AtomicBool::new(true));
        let tx = shared.tx.as_ref().expect("live hub has a command sender");
        if tx
            .send(Cmd::Add {
                id,
                sink,
                active: Arc::clone(&active),
            })
            .is_err()
        {
            deactivate_all(&mut shared);
            return Err(SubscribeError::Spawn(
                "feed hub command channel closed".into(),
            ));
        }
        shared.subscribers += 1;
        shared.active.insert(id, Arc::clone(&active));
        Ok(FeedSubscription {
            id,
            active,
            shared: Arc::clone(&self.shared),
        })
    }

    pub(crate) fn subscribers(&self) -> usize {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared.alive
            && shared
                .handle
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
        {
            deactivate_all(&mut shared);
        }
        shared.subscribers
    }
}

pub(crate) struct FeedSubscription {
    id: u64,
    active: Arc<AtomicBool>,
    shared: Arc<Mutex<Shared>>,
}

impl FeedSubscription {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn active_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.active)
    }
}

impl Drop for FeedSubscription {
    fn drop(&mut self) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared.active.remove(&self.id).is_some() {
            shared.subscribers = shared.subscribers.saturating_sub(1);
        }
        if shared.alive
            && let Some(tx) = &shared.tx
            && tx.send(Cmd::Remove(self.id)).is_err()
        {
            shared.alive = false;
            shared.tx = None;
        }
    }
}

async fn run(source: FeedSource, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Mutex<Shared>>) {
    let source_future = source();
    tokio::pin!(source_future);
    let mut sinks = BTreeMap::<u64, (FeedSink, Arc<AtomicBool>)>::new();
    let mut feed = loop {
        tokio::select! {
            command = rx.recv() => {
                if !apply_command(command, &mut sinks) {
                    finish_hub(&shared);
                    return;
                }
                if sinks.is_empty() && stop_if_empty(&mut rx, &mut sinks, &shared) {
                    return;
                }
            }
            result = &mut source_future => {
                let Some(feed) = result else {
                    deactivate_all(&mut shared.lock().unwrap_or_else(|error| error.into_inner()));
                    return;
                };
                break feed;
            }
        }
    };

    loop {
        if sinks.is_empty() {
            match rx.try_recv() {
                Ok(command) => {
                    if !apply_command(Some(command), &mut sinks) {
                        finish_hub(&shared);
                        return;
                    }
                    continue;
                }
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    if stop_hub_if_empty(&shared) {
                        return;
                    }
                }
            }
        }
        tokio::select! {
            command = rx.recv() => {
                if !apply_command(command, &mut sinks) {
                    finish_hub(&shared);
                    return;
                }
            }
            frame = feed.frames.recv() => match frame {
                None => {
                    deactivate_all(&mut shared.lock().unwrap_or_else(|error| error.into_inner()));
                    return;
                }
                Some(frame) if is_pane_or_hello(&frame) => {}
                Some(frame) => {
                    let payload = thegn_svc::control::http::frame_json(&frame);
                    let mut removed = Vec::new();
                    for (id, (sink, active)) in &mut sinks {
                        if !active.load(Ordering::Acquire) || !sink(&payload) {
                            active.store(false, Ordering::Release);
                            removed.push(*id);
                        }
                    }
                    for id in removed {
                        sinks.remove(&id);
                        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
                        if state.active.remove(&id).is_some() {
                            state.subscribers = state.subscribers.saturating_sub(1);
                        }
                    }
                }
            }
        }
    }
}

fn is_pane_or_hello(frame: &EventFrame) -> bool {
    matches!(
        frame,
        EventFrame::Hello(_) | EventFrame::PaneSnapshot { .. } | EventFrame::PaneDelta { .. }
    )
}

fn apply_command(
    command: Option<Cmd>,
    sinks: &mut BTreeMap<u64, (FeedSink, Arc<AtomicBool>)>,
) -> bool {
    match command {
        Some(Cmd::Add { id, sink, active }) => {
            if active.load(Ordering::Acquire) {
                sinks.insert(id, (sink, active));
            }
            true
        }
        Some(Cmd::Remove(id)) => {
            sinks.remove(&id);
            true
        }
        None => false,
    }
}

fn stop_if_empty(
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    sinks: &mut BTreeMap<u64, (FeedSink, Arc<AtomicBool>)>,
    shared: &Arc<Mutex<Shared>>,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(command) => {
                if !apply_command(Some(command), sinks) {
                    finish_hub(shared);
                    return true;
                }
                if !sinks.is_empty() {
                    return false;
                }
            }
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                return stop_hub_if_empty(shared);
            }
        }
    }
}

fn stop_hub_if_empty(shared: &Arc<Mutex<Shared>>) -> bool {
    let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
    if state.subscribers == 0 {
        state.alive = false;
        state.tx = None;
        true
    } else {
        false
    }
}

fn deactivate_all(shared: &mut Shared) {
    for active in shared.active.values() {
        active.store(false, Ordering::Release);
    }
    shared.active.clear();
    shared.subscribers = 0;
    shared.alive = false;
    shared.tx = None;
}

fn finish_hub(shared: &Arc<Mutex<Shared>>) {
    deactivate_all(&mut shared.lock().unwrap_or_else(|error| error.into_inner()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc as std_mpsc;
    use std::time::{Duration, Instant};

    struct DropSignal(std_mpsc::Sender<()>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.send(()).expect("drop probe receiver remains alive");
        }
    }

    fn test_feed() -> (mpsc::Sender<EventFrame>, Feed, std_mpsc::Receiver<()>) {
        let (tx, frames) = mpsc::channel(16);
        let (drop_tx, drop_rx) = std_mpsc::channel();
        (tx, Feed::new(frames, DropSignal(drop_tx)), drop_rx)
    }

    fn test_source(feeds: Vec<Feed>, calls: Arc<AtomicUsize>) -> FeedSource {
        let feeds = Arc::new(Mutex::new(VecDeque::from(feeds)));
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            let feed = feeds
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front();
            Box::pin(async move { feed })
        })
    }

    fn wait_inactive(subscription: &FeedSubscription) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while subscription.is_active() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            !subscription.is_active(),
            "subscription deactivates within five seconds"
        );
    }

    fn wait_thread_ended(hub: &FeedHub) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !hub
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert!(
            hub.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .handle
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished),
            "hub owner thread ends within five seconds"
        );
    }

    fn activity(name: &str) -> EventFrame {
        EventFrame::Activity {
            json: format!("{{\"name\":\"{name}\"}}"),
        }
    }

    #[test]
    fn two_subscribers_share_one_stream_and_both_receive_in_order() {
        let (frame_tx, feed, _drop_rx) = test_feed();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::clone(&calls)));
        let (first_tx, first_rx) = std_mpsc::channel();
        let (second_tx, second_rx) = std_mpsc::channel();
        let first = hub
            .subscribe(Box::new(move |value| first_tx.send(value.clone()).is_ok()))
            .unwrap();
        let second = hub
            .subscribe(Box::new(move |value| second_tx.send(value.clone()).is_ok()))
            .unwrap();
        frame_tx.blocking_send(activity("first")).unwrap();
        frame_tx.blocking_send(activity("second")).unwrap();
        let expected_first = thegn_svc::control::http::frame_json(&activity("first"));
        let expected_second = thegn_svc::control::http::frame_json(&activity("second"));
        assert_eq!(
            first_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            expected_first
        );
        assert_eq!(
            first_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            expected_second
        );
        assert_eq!(
            second_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            expected_first
        );
        assert_eq!(
            second_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            expected_second
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "two sinks use one source stream"
        );
        assert!(first.is_active() && second.is_active());
    }

    #[test]
    fn pane_and_hello_frames_never_reach_a_sink() {
        let (frame_tx, feed, _drop_rx) = test_feed();
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::new(AtomicUsize::new(0))));
        let (seen_tx, seen_rx) = std_mpsc::channel();
        let _sub = hub
            .subscribe(Box::new(move |value| seen_tx.send(value.clone()).is_ok()))
            .unwrap();
        frame_tx
            .blocking_send(EventFrame::Hello(thegn_core::control_wire::Hello {
                proto: 1,
                server: "test".into(),
                scopes: Vec::new(),
            }))
            .unwrap();
        frame_tx
            .blocking_send(EventFrame::PaneSnapshot {
                session: "s".into(),
                seq: 1,
                cols: 80,
                rows: 24,
                bytes: vec![1],
            })
            .unwrap();
        frame_tx
            .blocking_send(EventFrame::PaneDelta {
                session: "s".into(),
                seq: 2,
                bytes: vec![2],
            })
            .unwrap();
        let accepted = activity("accepted");
        let expected = thegn_svc::control::http::frame_json(&accepted);
        frame_tx.blocking_send(accepted).unwrap();
        assert_eq!(
            seen_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            expected
        );
        assert!(
            seen_rx.try_recv().is_err(),
            "pane bytes and hello are filtered"
        );
    }

    #[test]
    fn dropping_a_subscription_stops_its_sink_and_keeps_the_other() {
        let (frame_tx, feed, _drop_rx) = test_feed();
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::new(AtomicUsize::new(0))));
        let (dropped_tx, dropped_rx) = std_mpsc::channel();
        let dropped = hub
            .subscribe(Box::new(move |_| dropped_tx.send(()).is_ok()))
            .unwrap();
        let (kept_tx, kept_rx) = std_mpsc::channel();
        let kept = hub
            .subscribe(Box::new(move |_| kept_tx.send(()).is_ok()))
            .unwrap();
        drop(dropped);
        frame_tx.blocking_send(activity("live")).unwrap();
        kept_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("remaining sink receives the frame");
        assert!(
            dropped_rx.try_recv().is_err(),
            "dropped sink is not invoked"
        );
        assert!(kept.is_active());
    }

    #[test]
    fn last_unsubscribe_ends_the_thread_and_drops_the_feed() {
        let (_frame_tx, feed, dropped_rx) = test_feed();
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::new(AtomicUsize::new(0))));
        let subscription = hub.subscribe(Box::new(|_| true)).unwrap();
        drop(subscription);
        dropped_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("feed owner dropped after final unsubscribe");
        wait_thread_ended(&hub);
        assert_eq!(hub.subscribers(), 0);
    }

    #[test]
    fn resubscribe_after_the_hub_ended_calls_the_source_again() {
        let (first_tx, first_feed, first_drop) = test_feed();
        let (_second_tx, second_feed, _second_drop) = test_feed();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut hub = FeedHub::new(test_source(
            vec![first_feed, second_feed],
            Arc::clone(&calls),
        ));
        let first = hub.subscribe(Box::new(|_| true)).unwrap();
        drop(first);
        first_drop.recv_timeout(Duration::from_secs(5)).unwrap();
        wait_thread_ended(&hub);
        drop(first_tx);
        let second = hub.subscribe(Box::new(|_| true)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "new hub generation opens a new source"
        );
        assert!(second.is_active());
    }

    #[test]
    fn subscribe_beyond_the_cap_is_refused_full() {
        let (_frame_tx, feed, _drop_rx) = test_feed();
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::new(AtomicUsize::new(0))));
        let mut subscriptions = Vec::new();
        for _ in 0..MAX_FEED_SUBSCRIBERS {
            subscriptions.push(hub.subscribe(Box::new(|_| true)).unwrap());
        }
        assert!(matches!(
            hub.subscribe(Box::new(|_| true)),
            Err(SubscribeError::Full)
        ));
    }

    #[test]
    fn missing_daemon_deactivates_the_subscription() {
        let source: FeedSource = Arc::new(|| Box::pin(async { None }));
        let mut hub = FeedHub::new(source);
        let subscription = hub.subscribe(Box::new(|_| true)).unwrap();
        wait_inactive(&subscription);
        assert_eq!(hub.subscribers(), 0);
    }

    #[test]
    fn a_sink_returning_false_is_removed_without_affecting_others() {
        let (frame_tx, feed, _drop_rx) = test_feed();
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::new(AtomicUsize::new(0))));
        let failed = hub.subscribe(Box::new(|_| false)).unwrap();
        let (live_tx, live_rx) = std_mpsc::channel();
        let live = hub
            .subscribe(Box::new(move |_| live_tx.send(()).is_ok()))
            .unwrap();
        frame_tx.blocking_send(activity("one")).unwrap();
        live_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        wait_inactive(&failed);
        assert!(live.is_active());
        assert_eq!(hub.subscribers(), 1);
    }

    #[test]
    fn stream_end_deactivates_every_subscription() {
        let (frame_tx, feed, _drop_rx) = test_feed();
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::new(AtomicUsize::new(0))));
        let first = hub.subscribe(Box::new(|_| true)).unwrap();
        let second = hub.subscribe(Box::new(|_| true)).unwrap();
        drop(frame_tx);
        wait_inactive(&first);
        wait_inactive(&second);
        assert_eq!(hub.subscribers(), 0);
    }

    #[test]
    fn hub_thread_name_is_bounded_and_attributed() {
        let (frame_tx, feed, _drop_rx) = test_feed();
        let mut hub = FeedHub::new(test_source(vec![feed], Arc::new(AtomicUsize::new(0))));
        let (name_tx, name_rx) = std_mpsc::channel();
        let _subscription = hub
            .subscribe(Box::new(move |_| {
                let name = std::thread::current().name().unwrap_or_default().to_owned();
                name_tx.send(name).is_ok()
            }))
            .unwrap();
        frame_tx.blocking_send(activity("thread")).unwrap();
        let name = name_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(name, FEED_HUB_THREAD);
        assert!(name.len() <= 15);
    }
}
