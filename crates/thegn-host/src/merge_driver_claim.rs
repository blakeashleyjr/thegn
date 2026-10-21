use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use thegn_core::db::Db;
use thegn_core::store::{
    MERGE_CLAIM_MAX_LEASE_MS, MergeAttemptResult, MergeClaim, MergeClaimRequest, MergeStatusFields,
    WorktreeAuxStore,
};

use super::QueueItem;

const LEASE_MS: i64 = MERGE_CLAIM_MAX_LEASE_MS - 10_000;
const RENEW_EVERY: Duration = Duration::from_secs(60);

#[cfg(test)]
static JOINED_HEARTBEATS: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn heartbeat_joined(worktree: &str) -> bool {
    JOINED_HEARTBEATS
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(worktree)
}

pub(super) enum Admission {
    Claimed(RowClaim),
    HeldElsewhere(String),
    NotAdmitted(String),
}

pub(super) struct RowClaim {
    claim: Arc<Mutex<MergeClaim>>,
    heartbeat: Option<(JoinHandle<()>, mpsc::Sender<()>)>,
    lost: Arc<AtomicBool>,
}

impl RowClaim {
    pub(super) fn admit(
        db: &Db,
        item: &QueueItem,
        repo_path: &str,
        status: &str,
        target_branch: &str,
    ) -> anyhow::Result<Admission> {
        Self::admit_with(db, item, repo_path, status, target_branch, false)
    }

    pub(super) fn admit_unregistered(
        db: &Db,
        item: &QueueItem,
        repo_path: &str,
        status: &str,
        target_branch: &str,
    ) -> anyhow::Result<Admission> {
        Self::admit_with(db, item, repo_path, status, target_branch, true)
    }

    fn admit_with(
        db: &Db,
        item: &QueueItem,
        repo_path: &str,
        status: &str,
        target_branch: &str,
        unregistered: bool,
    ) -> anyhow::Result<Admission> {
        let request = MergeClaimRequest {
            worktree: item.worktree.clone(),
            branch: item.branch.clone(),
            repo_root: repo_path.to_owned(),
            target_branch: target_branch.to_owned(),
            location: item.location.clone(),
            expected_status: status.to_owned(),
            lease_until_ms: thegn_core::util::now_ms() + LEASE_MS,
            owner_hint: Some(format!("thegn pid {}", std::process::id())),
        };
        let admission_result = if unregistered {
            db.admit_unregistered_local_merge_claim(&request)?
        } else {
            db.admit_merge_claim(&request)?
        };
        let Some(claim) = admission_result else {
            let view = db.observe_merge_claim(&item.worktree)?;
            let admission = match view {
                Some(view)
                    if view.phase != "unclaimed"
                        && view
                            .lease_until_ms
                            .is_some_and(|until| until > thegn_core::util::now_ms()) =>
                {
                    Admission::HeldElsewhere(format!(
                        "{}: claimed by another drain (generation {}, until {}); skipped",
                        item.branch,
                        view.generation,
                        view.lease_until_ms.unwrap_or_default()
                    ))
                }
                _ => Admission::NotAdmitted(format!(
                    "{}: not admitted ({} since the queue was listed); skipped",
                    item.branch,
                    if unregistered {
                        "the row was registered or changed"
                    } else {
                        "registry identity or status changed"
                    }
                )),
            };
            return Ok(admission);
        };
        let claim = Arc::new(Mutex::new(claim));
        let lost = Arc::new(AtomicBool::new(false));
        let heartbeat = db.file_path().map(|path| {
            let (stop_tx, stop_rx) = mpsc::channel();
            let thread_claim = Arc::clone(&claim);
            let thread_lost = Arc::clone(&lost);
            let handle = thegn_core::thread_name::spawn_named("thegn-mqlease", move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let db = match Db::open_at(&path) {
                    Ok(db) => db,
                    Err(error) => {
                        tracing::debug!(target: "thegn::merge", %error, "merge claim heartbeat could not open its database");
                        thread_lost.store(true, Ordering::Release);
                        return;
                    }
                };
                loop {
                    match stop_rx.recv_timeout(RENEW_EVERY) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let mut current = thread_claim
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            match db.renew_merge_claim(
                                &current,
                                thegn_core::util::now_ms() + LEASE_MS,
                            ) {
                                Ok(renewed) => *current = renewed,
                                Err(error) => {
                                    tracing::debug!(target: "thegn::merge", %error, "merge claim heartbeat lost ownership");
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
        Ok(Admission::Claimed(Self {
            claim,
            heartbeat,
            lost,
        }))
    }

    pub(super) fn write(
        &self,
        db: &Db,
        status: thegn_core::attention::MqStatus,
        fields: &MergeStatusFields,
    ) -> anyhow::Result<()> {
        let mut claim = self
            .claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match db.replace_merge_status_claimed(&claim, status.as_str(), fields) {
            Ok(updated) => *claim = updated,
            Err(error) => {
                if error
                    .to_string()
                    .contains(thegn_core::store::MERGE_CLAIM_LOST)
                {
                    self.lost.store(true, Ordering::Release);
                }
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn consume_attempt(&self, db: &Db, max: u32) -> anyhow::Result<MergeAttemptResult> {
        let claim = self
            .claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = match db.consume_merge_agent_attempt(&claim, max) {
            Ok(result) => result,
            Err(error) => {
                if error
                    .to_string()
                    .contains(thegn_core::store::MERGE_CLAIM_LOST)
                {
                    self.lost.store(true, Ordering::Release);
                }
                return Err(error);
            }
        };
        if result == MergeAttemptResult::ClaimLost {
            self.lost.store(true, Ordering::Release);
        }
        Ok(result)
    }

    pub(super) fn refund_attempt(&self, db: &Db) -> anyhow::Result<bool> {
        let claim = self
            .claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = match db.refund_merge_agent_attempt(&claim) {
            Ok(result) => result,
            Err(error) => {
                if error
                    .to_string()
                    .contains(thegn_core::store::MERGE_CLAIM_LOST)
                {
                    self.lost.store(true, Ordering::Release);
                }
                return Err(error);
            }
        };
        if !result {
            self.lost.store(true, Ordering::Release);
        }
        Ok(result)
    }

    pub(super) fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub(super) fn finish(mut self, db: &Db) -> anyhow::Result<()> {
        self.stop_heartbeat();
        if self.lost() {
            anyhow::bail!(thegn_core::store::MERGE_CLAIM_LOST);
        }
        let mut claim = self
            .claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *claim = db.settle_merge_claim_no_child(&claim)?;
        db.release_merge_claim(&claim)
    }

    fn stop_heartbeat(&mut self) {
        if let Some((handle, stop_tx)) = self.heartbeat.take() {
            if let Err(error) = stop_tx.send(()) {
                tracing::debug!(target: "thegn::merge", %error, "best-effort: merge claim heartbeat stop signal was unavailable");
            }
            if handle.join().is_err() {
                tracing::debug!(target: "thegn::merge", "best-effort: merge claim heartbeat thread panicked");
            }
            #[cfg(test)]
            JOINED_HEARTBEATS
                .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    self.claim
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .worktree
                        .clone(),
                );
        }
    }
}

impl Drop for RowClaim {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}
