use gtui_core::datasource::TimeRange;
use gtui_core::frame::Frame;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct ObserveApp {
    pub time_range: TimeRange,
    pub query_rx: mpsc::UnboundedReceiver<Vec<Frame>>,
    waker: Arc<dyn Fn() + Send + Sync>,
}

impl ObserveApp {
    pub fn new(
        time_range: TimeRange,
        waker: Arc<dyn Fn() + Send + Sync>,
    ) -> (Self, mpsc::UnboundedSender<Vec<Frame>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                time_range,
                query_rx: rx,
                waker,
            },
            tx,
        )
    }

    pub fn tick(&mut self) -> Option<Vec<Frame>> {
        // Drain ONE message to process (or drain all in a real app)
        // If we get a result, we return it so the UI can update.
        if let Ok(frames) = self.query_rx.try_recv() {
            // Pulse waker for re-render in the actual event loop.
            (self.waker)();
            return Some(frames);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_app_waker_and_tick() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        let waker = Arc::new(move || {
            called_clone.store(true, Ordering::SeqCst);
        });

        let tr = TimeRange {
            from: Utc::now(),
            to: Utc::now(),
        };

        let (mut app, tx) = ObserveApp::new(tr, waker);

        // Before sending, tick should return None
        assert!(app.tick().is_none());
        assert!(!called.load(Ordering::SeqCst));

        // Send a result
        tx.send(vec![]).unwrap();

        // Tick should receive it and call waker
        let res = app.tick();
        assert!(res.is_some());
        assert!(called.load(Ordering::SeqCst));
    }
}
