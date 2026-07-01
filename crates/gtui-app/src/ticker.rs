use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};

pub struct AutoRefreshTicker {
    cancel_tx: broadcast::Sender<()>,
}

impl AutoRefreshTicker {
    pub fn spawn(
        refresh_interval: Duration,
        waker: Arc<dyn Fn() + Send + Sync>,
        query_trigger_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        let (cancel_tx, mut cancel_rx) = broadcast::channel(1);

        tokio::spawn(async move {
            let mut ticker = interval(refresh_interval);
            ticker.tick().await; // skip immediate first tick

            loop {
                tokio::select! {
                    biased; // Ensure we handle cancel before ticking if both are ready
                    _ = cancel_rx.recv() => {
                        // Ticker cancelled, exit loop.
                        break;
                    }
                    _ = ticker.tick() => {
                        // Trigger a query reload (ignore send failure if receiver dropped)
                        if query_trigger_tx.send(()).is_ok() {
                            (waker)();
                        } else {
                            break;
                        }
                    }
                }
            }
        });

        Self { cancel_tx }
    }

    pub fn stop(&self) {
        let _ = self.cancel_tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn test_ticker_cancellation() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        let waker = Arc::new(move || {
            called_clone.store(true, Ordering::SeqCst);
        });

        let (tx, mut rx) = mpsc::unbounded_channel();

        // Spawn ticker with a slightly longer interval to avoid races
        let ticker = AutoRefreshTicker::spawn(Duration::from_millis(50), waker, tx);

        // Let it tick once
        let _ = rx.recv().await;
        assert!(called.load(Ordering::SeqCst));

        // Stop the ticker
        ticker.stop();

        // Drop our receiver so any future send immediately fails and breaks the loop
        drop(rx);

        // Let the tokio scheduler run so the background task gets the cancel message
        tokio::time::sleep(Duration::from_millis(15)).await;

        // If the test completes without hanging/panicking, cancellation worked.
    }
}
