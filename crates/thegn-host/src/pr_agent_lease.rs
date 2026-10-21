use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use thegn_core::db::Db;
use thegn_core::pr_queue::{PR_AGENT_MAX_LEASE_MS, PrAgentAdmit, PrAgentLease, PrAgentRefusal};
use thegn_core::store::WorktreeAuxStore;

const LEASE_MS: i64 = PR_AGENT_MAX_LEASE_MS - 10_000;
const RENEW_EVERY: Duration = Duration::from_secs(60);

pub(crate) fn lease_until_ms() -> i64 {
    thegn_core::util::now_ms() + LEASE_MS
}

pub(crate) fn owner_hint() -> String {
    format!("thegn pid {}", std::process::id())
}

pub(crate) struct PrAgentGuard {
    lease: Arc<Mutex<PrAgentLease>>,
    heartbeat: Option<(JoinHandle<()>, mpsc::Sender<()>)>,
    lost: Arc<AtomicBool>,
}

impl PrAgentGuard {
    pub(crate) fn admit(
        db: &Db,
        request: &PrAgentAdmit<'_>,
    ) -> anyhow::Result<Result<Self, PrAgentRefusal>> {
        let lease = match db.admit_pr_agent(request)? {
            thegn_core::pr_queue::PrAgentAdmission::Admitted(lease) => lease,
            thegn_core::pr_queue::PrAgentAdmission::Refused(refusal) => return Ok(Err(refusal)),
        };
        let lease = Arc::new(Mutex::new(lease));
        let lost = Arc::new(AtomicBool::new(false));
        let heartbeat = db.file_path().map(|path| {
            let (stop_tx, stop_rx) = mpsc::channel();
            let thread_lease = Arc::clone(&lease);
            let thread_lost = Arc::clone(&lost);
            let handle = thegn_core::thread_name::spawn_named("thegn-prlease", move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let db = match Db::open_at(&path) {
                    Ok(db) => db,
                    Err(error) => {
                        tracing::debug!(target: "thegn::prq", %error, "PR-agent lease heartbeat could not open its database");
                        thread_lost.store(true, Ordering::Release);
                        return;
                    }
                };
                loop {
                    match stop_rx.recv_timeout(RENEW_EVERY) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let mut current = thread_lease.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match db.renew_pr_agent(&current, lease_until_ms()) {
                                Ok(renewed) => *current = renewed,
                                Err(error) => {
                                    tracing::debug!(target: "thegn::prq", %error, "PR-agent lease heartbeat lost ownership");
                                    thread_lost.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }
                    }
                }
            });
            (handle, stop_tx)
        });
        Ok(Ok(Self {
            lease,
            heartbeat,
            lost,
        }))
    }

    pub(crate) fn attempt(&self) -> u32 {
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .attempt
    }

    pub(crate) fn finish(
        mut self,
        db: &Db,
        finished: Option<(&str, &str)>,
    ) -> anyhow::Result<bool> {
        self.stop_heartbeat();
        if self.lost.load(Ordering::Acquire) {
            return Ok(false);
        }
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        db.release_pr_agent(&lease, finished)
    }

    fn stop_heartbeat(&mut self) {
        if let Some((handle, stop_tx)) = self.heartbeat.take() {
            if let Err(error) = stop_tx.send(()) {
                tracing::debug!(target: "thegn::prq", %error, "PR-agent lease heartbeat stop signal was unavailable");
            }
            if handle.join().is_err() {
                tracing::debug!(target: "thegn::prq", "PR-agent lease heartbeat thread panicked");
            }
        }
    }
}

impl Drop for PrAgentGuard {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::pr_queue::PrAgentAdmission;

    #[test]
    fn finish_releases_and_a_lost_lease_reports_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr-agent.db");
        let db = Db::open_at(&path).unwrap();
        db.enqueue_bound_pr(
            "/repo",
            1,
            Some("/repo/wt"),
            "feature",
            "main",
            "github",
            "https://github.com/me/repo/pull/1",
        )
        .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        db.enqueue_bound_pr(
            "/repo",
            2,
            Some("/repo/wt"),
            "feature-2",
            "main",
            "github",
            "https://github.com/me/repo/pull/2",
        )
        .unwrap();
        let successful_row = db
            .list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|queued| queued.number == 2)
            .unwrap();
        let successful_request = PrAgentAdmit {
            key: &successful_row.key,
            expected_attempts: 0,
            expected_checkout_gen: successful_row.checkout_gen,
            max_attempts: 3,
            head: "head-success",
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: lease_until_ms(),
            owner_hint: Some("test"),
            ci_candidate: None,
        };
        let successful_guard = PrAgentGuard::admit(&db, &successful_request)
            .unwrap()
            .unwrap();
        assert!(successful_guard.finish(&db, None).unwrap());
        let request = PrAgentAdmit {
            key: &row.key,
            expected_attempts: 0,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: "head-a",
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: thegn_core::util::now_ms() + 250,
            owner_hint: Some("test"),
            ci_candidate: None,
        };
        let old = match PrAgentGuard::admit(&db, &request).unwrap() {
            Ok(guard) => guard,
            Err(refusal) => panic!("unexpected refusal: {refusal}"),
        };
        std::thread::sleep(Duration::from_millis(300));
        let other = Db::open_at(&path).unwrap();
        let request = PrAgentAdmit {
            key: &row.key,
            expected_attempts: 1,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: "head-b",
            blocker: "ci",
            detail: "takeover",
            lease_until_ms: lease_until_ms(),
            owner_hint: Some("second"),
            ci_candidate: None,
        };
        let new = match other.admit_pr_agent(&request).unwrap() {
            PrAgentAdmission::Admitted(lease) => lease,
            result => panic!("expected takeover, got {result:?}"),
        };
        assert!(!old.finish(&db, None).unwrap());
        assert!(other.release_pr_agent(&new, None).unwrap());
    }
}
