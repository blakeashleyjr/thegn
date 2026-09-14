//! Retry only attachment of an existing session. Fresh opens have side effects
//! and an ambiguous failed/cancelled open must never be automatically repeated.

use crate::pane_source::ExecSource;
use anyhow::anyhow;
use std::time::Duration;
use thegn_svc::provider::{ExecControl, ExecSession};
use tokio::sync::mpsc;

pub(crate) enum Recovery {
    Attached(ExecSession),
    Absent,
    Cancelled,
    Exhausted(anyhow::Error),
}

#[derive(Clone, Copy)]
pub(crate) struct Policy {
    pub attempts: u32,
    pub attempt_timeout: Duration,
    pub budget: Duration,
    pub backoff: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            attempts: 6,
            attempt_timeout: Duration::from_secs(8),
            budget: Duration::from_secs(30),
            backoff: Duration::from_millis(250),
        }
    }
}

/// Controls remain in the existing bounded receiver, including resize ordering.
/// Nothing is consumed/replayed during recovery and producers retain the same
/// Full/backpressure behavior. Receiver::is_closed sees owner drop even when the
/// input queue is full; the short timer exists only while recovery is in flight.
async fn cancellable<T>(
    future: impl std::future::Future<Output = T>,
    controls: &mpsc::Receiver<ExecControl>,
) -> Option<T> {
    tokio::pin!(future);
    loop {
        if controls.is_closed() {
            return None;
        }
        tokio::select! {
            result = &mut future => return Some(result),
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

pub(crate) async fn backoff(delay: Duration, controls: &mpsc::Receiver<ExecControl>) -> bool {
    cancellable(tokio::time::sleep(delay), controls)
        .await
        .is_some()
}

pub(crate) async fn recover(
    source: &dyn ExecSource,
    id: &str,
    cols: u16,
    rows: u16,
    controls: &mpsc::Receiver<ExecControl>,
    policy: Policy,
) -> Recovery {
    let deadline = tokio::time::Instant::now() + policy.budget;
    let mut last_error = anyhow!("session recovery budget exhausted");
    for attempt in 0..policy.attempts {
        if attempt > 0 {
            let delay = policy.backoff.saturating_mul(1 << (attempt - 1).min(4));
            let delay = delay.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
            if !backoff(delay, controls).await {
                return Recovery::Cancelled;
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let operation = async {
            match source.attach(id, cols, rows).await {
                Ok(session) => Ok(Recovery::Attached(session)),
                Err(attach_error) => match source.session_absent(id).await {
                    Ok(true) => Ok(Recovery::Absent),
                    Ok(false) => Err(attach_error),
                    Err(query_error) => Err(attach_error.context(format!(
                        "session absence could not be established: {query_error}"
                    ))),
                },
            }
        };
        let timed = tokio::time::timeout(policy.attempt_timeout.min(remaining), operation);
        match cancellable(timed, controls).await {
            None => return Recovery::Cancelled,
            Some(Ok(Ok(recovered))) => return recovered,
            Some(Ok(Err(error))) => last_error = error,
            Some(Err(_)) => last_error = anyhow!("session attach/roster attempt timed out"),
        }
    }
    Recovery::Exhausted(last_error)
}

pub(crate) async fn close_owned(source: &dyn ExecSource, id: Option<String>, detached: bool) {
    if !detached && let Some(id) = id {
        match tokio::time::timeout(Duration::from_secs(3), source.kill_session(&id)).await {
            Ok(Ok(())) => {}
            result => tracing::debug!(target: "thegn::daemon", session = %id, ?result,
                "close-time session kill did not complete"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use std::sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    };
    use thegn_svc::provider::{ExecFrame, ExecSpec};

    struct Source {
        failures: u32,
        calls: AtomicU32,
        session: Mutex<Option<ExecSession>>,
        absent: bool,
        query_error: bool,
        hung: bool,
        kills: Mutex<Vec<String>>,
    }

    fn source(failures: u32) -> Source {
        let (_frames, frames) = mpsc::channel::<ExecFrame>(1);
        let (control, _commands) = mpsc::channel(1);
        let (_id, session_id) = tokio::sync::watch::channel(Some("same-id".into()));
        Source {
            failures,
            calls: AtomicU32::new(0),
            session: Mutex::new(Some(ExecSession {
                frames,
                control,
                session_id,
            })),
            absent: false,
            query_error: false,
            hung: false,
            kills: Mutex::new(Vec::new()),
        }
    }

    impl ExecSource for Source {
        fn open<'a>(&'a self, _: &'a ExecSpec) -> BoxFuture<'a, anyhow::Result<ExecSession>> {
            Box::pin(async { panic!("recovery must never perform an unproven fresh open") })
        }
        fn attach<'a>(
            &'a self,
            id: &'a str,
            _: u16,
            _: u16,
        ) -> BoxFuture<'a, anyhow::Result<ExecSession>> {
            Box::pin(async move {
                assert_eq!(id, "same-id");
                let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
                if self.hung {
                    std::future::pending::<()>().await;
                }
                if attempt < self.failures {
                    return Err(anyhow!("temporary transport failure"));
                }
                self.session
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| anyhow!("no session"))
            })
        }
        fn session_absent<'a>(&'a self, _: &'a str) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                if self.query_error {
                    Err(anyhow!("roster unavailable"))
                } else {
                    Ok(self.absent)
                }
            })
        }
        fn kill_session<'a>(&'a self, id: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.kills.lock().unwrap().push(id.into());
                Ok(())
            })
        }
    }

    fn policy() -> Policy {
        Policy {
            attempts: 4,
            attempt_timeout: Duration::from_millis(25),
            budget: Duration::from_millis(150),
            backoff: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn transient_recovery_keeps_exact_id_and_existing_control_backpressure() {
        let source = source(2);
        let (tx, mut rx) = mpsc::channel(2);
        let input = ExecControl::Stdin(b"pending".to_vec());
        let resize = ExecControl::Resize {
            cols: 120,
            rows: 40,
        };
        tx.try_send(input.clone()).unwrap();
        tx.try_send(resize.clone()).unwrap();
        let result = recover(&source, "same-id", 80, 24, &rx, policy()).await;
        let Recovery::Attached(session) = result else {
            panic!("did not recover")
        };
        assert_eq!(session.session_id.borrow().as_deref(), Some("same-id"));
        assert_eq!(source.calls.load(Ordering::SeqCst), 3);
        assert!(matches!(
            tx.try_send(input.clone()),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert_eq!(rx.try_recv().unwrap(), input);
        assert_eq!(rx.try_recv().unwrap(), resize);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_absence_retries_to_budget_but_proven_absence_stops() {
        let (tx, rx) = mpsc::channel(1);
        let mut unknown = source(100);
        unknown.query_error = true;
        assert!(matches!(
            recover(&unknown, "same-id", 80, 24, &rx, policy()).await,
            Recovery::Exhausted(_)
        ));
        assert_eq!(unknown.calls.load(Ordering::SeqCst), policy().attempts);
        let mut absent = source(100);
        absent.absent = true;
        assert!(matches!(
            recover(&absent, "same-id", 80, 24, &rx, policy()).await,
            Recovery::Absent
        ));
        assert_eq!(absent.calls.load(Ordering::SeqCst), 1);
        drop(tx);
    }

    #[tokio::test]
    async fn hung_attach_is_time_bounded() {
        let (_tx, rx) = mpsc::channel(1);
        let mut hung = source(0);
        hung.hung = true;
        let start = tokio::time::Instant::now();
        assert!(matches!(
            recover(&hung, "same-id", 80, 24, &rx, policy()).await,
            Recovery::Exhausted(_)
        ));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(hung.calls.load(Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn dropping_owner_cancels_hung_attach_even_with_buffered_input() {
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(ExecControl::Stdin(b"queued".to_vec())).unwrap();
        let mut hung = source(0);
        hung.hung = true;
        let long = Policy {
            attempt_timeout: Duration::from_secs(20),
            budget: Duration::from_secs(30),
            ..policy()
        };
        let (recovery, ()) = tokio::join!(recover(&hung, "same-id", 80, 24, &rx, long), async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            drop(tx);
        },);
        assert!(matches!(recovery, Recovery::Cancelled));
        assert_eq!(hung.calls.load(Ordering::SeqCst), 1);
        close_owned(&hung, Some("same-id".into()), true).await;
        assert!(
            hung.kills.lock().unwrap().is_empty(),
            "detach must preserve the shell"
        );
        close_owned(&hung, Some("same-id".into()), false).await;
        assert_eq!(*hung.kills.lock().unwrap(), vec!["same-id"]);
    }

    #[tokio::test]
    async fn dropping_owner_interrupts_long_backoff() {
        let (tx, rx) = mpsc::channel(1);
        let start = tokio::time::Instant::now();
        let (completed, ()) = tokio::join!(backoff(Duration::from_secs(30), &rx), async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            drop(tx);
        },);
        assert!(!completed);
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
