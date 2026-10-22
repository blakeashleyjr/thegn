//! A single owned worker thread with a bounded request queue for blocking
//! SMTC/WinRT calls; pure and Linux-tested.

use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use crate::MediaError;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub(crate) struct OpWorker {
    tx: Option<SyncSender<Job>>,
    handle: Option<std::thread::JoinHandle<()>>,
    name: &'static str,
    capacity: usize,
}

pub(crate) fn spawn(name: &'static str, capacity: usize) -> Result<OpWorker, MediaError> {
    let (tx, rx) = sync_channel::<Job>(capacity);
    let handle = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                    tracing::warn!(target: "thegn::media", worker = name, "media worker job panicked");
                }
            }
        })
        .map_err(|error| MediaError::Unavailable(format!("{name} worker: {error}")))?;
    Ok(OpWorker {
        tx: Some(tx),
        handle: Some(handle),
        name,
        capacity,
    })
}

impl OpWorker {
    pub(crate) fn submit<T, F>(
        &self,
        f: F,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<T, MediaError>>, MediaError>
    where
        F: FnOnce() -> Result<T, MediaError> + Send + 'static,
        T: Send + 'static,
    {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move || {
            if reply.is_closed() {
                return;
            }
            let out = f();
            if reply.send(out).is_err() {
                tracing::debug!(target: "thegn::media", "media worker reply dropped");
            }
        });
        let Some(tx) = self.tx.as_ref() else {
            return Err(MediaError::Backend(format!("{} stopped", self.name)));
        };
        match tx.try_send(job) {
            Ok(()) => Ok(rx),
            Err(TrySendError::Full(_)) => Err(MediaError::Backend(format!(
                "{} busy: {} operations already queued",
                self.name, self.capacity
            ))),
            Err(TrySendError::Disconnected(_)) => {
                Err(MediaError::Backend(format!("{} stopped", self.name)))
            }
        }
    }

    pub(crate) async fn run<T, F>(&self, deadline: Duration, f: F) -> Result<T, MediaError>
    where
        F: FnOnce() -> Result<T, MediaError> + Send + 'static,
        T: Send + 'static,
    {
        let rx = self.submit(f)?;
        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MediaError::Backend(format!(
                "{} operation failed before replying",
                self.name
            ))),
            Err(_) => Err(MediaError::Backend(format!(
                "{} operation timed out after {} ms",
                self.name,
                deadline.as_millis()
            ))),
        }
    }

    #[cfg(test)]
    fn close(&mut self) {
        drop(self.tx.take());
    }

    #[cfg(test)]
    fn stop_and_join(mut self) -> std::thread::Result<()> {
        drop(self.tx.take());
        match self.handle.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }
}

impl Drop for OpWorker {
    fn drop(&mut self) {
        drop(self.tx.take());
        drop(self.handle.take());
    }
}

#[cfg(test)]
mod tests {
    use super::spawn;
    use crate::MediaError;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[tokio::test]
    async fn ops_run_serially_on_one_named_thread() {
        let worker = spawn("thegn-smtc-test", 4).unwrap();
        let mut ids = Vec::new();
        for _ in 0..5 {
            let (name, id) = worker
                .run(Duration::from_secs(2), || {
                    Ok((
                        std::thread::current().name().map(str::to_owned),
                        std::thread::current().id(),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(name.as_deref(), Some("thegn-smtc-test"));
            ids.push(id);
        }
        assert!(ids.iter().all(|id| *id == ids[0]));
        worker.stop_and_join().unwrap();
    }

    #[tokio::test]
    async fn never_returning_op_times_out_and_later_ops_are_refused_not_threaded() {
        let worker = spawn("thegn-smtc-test", 1).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel();
        let first = worker.submit(move || {
            started_tx.send(()).unwrap();
            gate_rx.recv().unwrap();
            Ok(())
        });
        assert!(first.is_ok());
        started_rx.recv().unwrap();
        let timed_out_ran = Arc::new(AtomicBool::new(false));
        let timed_out_job_ran = Arc::clone(&timed_out_ran);
        let result = worker
            .run(Duration::from_millis(50), move || {
                timed_out_job_ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(
            matches!(result, Err(MediaError::Backend(message)) if message.contains("timed out"))
        );
        let further = worker.submit(|| Ok(()));
        assert!(matches!(further, Err(MediaError::Backend(message)) if message.contains("busy")));
        gate_tx.send(()).unwrap();
        worker.run(Duration::from_secs(2), || Ok(())).await.unwrap();
        assert!(!timed_out_ran.load(Ordering::SeqCst));
        worker.stop_and_join().unwrap();
    }

    #[tokio::test]
    async fn abandoned_queued_op_is_skipped() {
        let worker = spawn("thegn-smtc-test", 2).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel();
        let first = worker.submit(move || {
            started_tx.send(()).unwrap();
            gate_rx.recv().unwrap();
            Ok(())
        });
        assert!(first.is_ok());
        started_rx.recv().unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_job = Arc::clone(&ran);
        let abandoned = worker.submit(move || {
            ran_job.store(true, Ordering::SeqCst);
            Ok(())
        });
        let receiver = abandoned.unwrap();
        drop(receiver);
        gate_tx.send(()).unwrap();
        worker.run(Duration::from_secs(2), || Ok(())).await.unwrap();
        assert!(!ran.load(Ordering::SeqCst));
        worker.stop_and_join().unwrap();
    }

    #[tokio::test]
    #[allow(
        clippy::panic,
        reason = "the worker must contain a panicking operation"
    )]
    async fn panicking_op_reports_an_error_and_the_worker_survives() {
        let worker = spawn("thegn-smtc-test", 2).unwrap();
        let before_panic_id = worker
            .run(Duration::from_secs(2), || Ok(std::thread::current().id()))
            .await
            .unwrap();
        let failed = worker
            .run(Duration::from_secs(2), || -> Result<(), MediaError> {
                panic!("operation panic")
            })
            .await;
        assert!(
            matches!(failed, Err(MediaError::Backend(message)) if message.contains("failed before replying"))
        );
        let first_id = worker
            .run(Duration::from_secs(2), || Ok(std::thread::current().id()))
            .await
            .unwrap();
        let second_id = worker
            .run(Duration::from_secs(2), || Ok(std::thread::current().id()))
            .await
            .unwrap();
        assert_eq!(before_panic_id, first_id);
        assert_eq!(first_id, second_id);
        worker.stop_and_join().unwrap();
    }

    #[test]
    fn dropping_the_worker_stops_its_thread() {
        let worker = spawn("thegn-smtc-test", 1).unwrap();
        worker.stop_and_join().unwrap();

        let mut worker = spawn("thegn-smtc-test", 1).unwrap();
        worker.close();
        let result = worker.submit(|| Ok(()));
        assert!(matches!(result, Err(MediaError::Backend(message)) if message.contains("stopped")));
        worker.stop_and_join().unwrap();
    }
}
