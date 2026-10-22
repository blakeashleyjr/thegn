use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use thegn_core::db::Db;
use thegn_core::pr_queue::{
    PR_AGENT_MAX_LEASE_MS, PrAgentAdmit, PrAgentLease, PrAgentLeaseView, PrAgentRefusal,
    expired_pr_agent_detail, pr_agent_lease_live,
};
use thegn_core::store::WorktreeAuxStore;

const LEASE_MS: i64 = PR_AGENT_MAX_LEASE_MS - 10_000;
const RENEW_EVERY: Duration = Duration::from_secs(60);

fn heartbeat_failure_is_terminal(error: &anyhow::Error, lease_until_ms: i64, now_ms: i64) -> bool {
    error
        .to_string()
        .contains(thegn_core::pr_queue::PR_AGENT_LEASE_LOST)
        || now_ms >= lease_until_ms
}

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
        Self::admit_with_renew_every(db, request, RENEW_EVERY)
    }

    fn admit_with_renew_every(
        db: &Db,
        request: &PrAgentAdmit<'_>,
        renew_every: Duration,
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
                loop {
                    match stop_rx.recv_timeout(renew_every) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let mut current = thread_lease.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            let result = Db::open_at(&path)
                                .and_then(|db| db.renew_pr_agent(&current, lease_until_ms()));
                            match result {
                                Ok(renewed) => *current = renewed,
                                Err(error) => {
                                    let terminal = heartbeat_failure_is_terminal(
                                        &error,
                                        current.lease_until_ms,
                                        thegn_core::util::now_ms(),
                                    );
                                    if terminal {
                                        tracing::debug!(target: "thegn::prq", %error, "PR-agent lease heartbeat confirmed lease loss or reached its expiry");
                                        thread_lost.store(true, Ordering::Release);
                                        break;
                                    }
                                    tracing::debug!(target: "thegn::prq", %error, lease_until_ms = current.lease_until_ms, "PR-agent lease heartbeat could not confirm ownership; retrying before lease expiry");
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

    pub(crate) fn cancel_flag(&self) -> &AtomicBool {
        &self.lost
    }

    pub(crate) fn lease_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub(crate) fn record_own_head(
        &self,
        db: &Db,
        admitted: &str,
        own: &str,
    ) -> anyhow::Result<bool> {
        if self.lost.load(Ordering::Acquire) {
            return Ok(false);
        }
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        db.record_pr_agent_head(&lease, admitted, own)
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

pub(crate) fn observe_own_head(
    loc: &thegn_core::remote::GitLoc,
    admitted: &str,
    remote_head: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let thegn_core::remote::GitLoc::Local(path) = loc else {
        return None;
    };
    let local = loc.git_out(&["rev-parse", "HEAD"])?;
    if local == admitted {
        return None;
    }
    let descends = thegn_core::util::git_is_ancestor(path, admitted, &local);
    if descends != Some(true) {
        return None;
    }
    thegn_core::pr_queue::own_agent_head(admitted, Some(&local), remote_head().as_deref(), descends)
}

pub(crate) fn own_head_record_warning(number: u64, result: anyhow::Result<bool>) -> Option<String> {
    match result {
        Ok(true) => None,
        Ok(false) => Some(format!(
            "PR #{number}: the agent's push could not be recorded under its lease; the next refresh treats it as a foreign push"
        )),
        Err(error) => Some(format!(
            "PR #{number}: the agent's push could not be recorded under its lease; the next refresh treats it as a foreign push ({error})"
        )),
    }
}

pub(crate) fn reconcile_expired(
    db: &Db,
    key: &str,
    number: u64,
    view: &PrAgentLeaseView,
) -> anyhow::Result<Option<String>> {
    if pr_agent_lease_live(Some(view), thegn_core::util::now_ms()) {
        return Ok(None);
    }
    let detail = expired_pr_agent_detail(view);
    if db.reap_expired_pr_agent(key, view.generation, &detail)? {
        Ok(Some(format!("PR #{number}: {detail}")))
    } else {
        Ok(None)
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

    #[test]
    fn lost_lease_raises_the_guard_cancel_flag() {
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
        let request = PrAgentAdmit {
            key: &row.key,
            expected_attempts: 0,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: "head-a",
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: lease_until_ms(),
            owner_hint: Some("test"),
            ci_candidate: None,
        };
        let guard = PrAgentGuard::admit_with_renew_every(&db, &request, Duration::from_millis(20))
            .unwrap()
            .unwrap();
        assert!(!guard.lease_lost());
        let copy = guard.lease.lock().unwrap().clone();
        assert!(
            Db::open_at(&path)
                .unwrap()
                .release_pr_agent(&copy, None)
                .unwrap()
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !guard.cancel_flag().load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(guard.cancel_flag().load(Ordering::Acquire));
        assert!(!guard.finish(&db, None).unwrap());
    }

    #[test]
    fn live_lease_keeps_the_guard_cancel_flag_clear() {
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
        let request = PrAgentAdmit {
            key: &row.key,
            expected_attempts: 0,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: "head-a",
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: lease_until_ms(),
            owner_hint: Some("test"),
            ci_candidate: None,
        };
        let guard = PrAgentGuard::admit_with_renew_every(&db, &request, Duration::from_millis(20))
            .unwrap()
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(!guard.cancel_flag().load(Ordering::Acquire));
        assert!(guard.finish(&db, None).unwrap());
    }

    #[test]
    fn heartbeat_retries_transient_errors_until_the_lease_expires() {
        let transient = anyhow::anyhow!("database is busy");
        assert!(!heartbeat_failure_is_terminal(&transient, 200, 199));
        assert!(heartbeat_failure_is_terminal(&transient, 200, 200));

        let lost = anyhow::anyhow!("{}", thegn_core::pr_queue::PR_AGENT_LEASE_LOST);
        assert!(heartbeat_failure_is_terminal(&lost, 500, 100));
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture runs isolated local git commands"
    )]
    #[test]
    fn observe_own_head_attributes_only_a_pushed_descendant() {
        use std::cell::Cell;
        let dir = tempfile::tempdir().unwrap();
        git_fixture_repo(dir.path());
        let run = |args: &[&str]| {
            let status = thegn_core::util::git_cmd(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "git {args:?} failed in {}",
                dir.path().display()
            );
        };
        let head = || {
            thegn_core::util::git_cmd(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout
        };
        let admitted = String::from_utf8(head()).unwrap().trim().to_owned();
        std::fs::write(dir.path().join("tracked"), "agent change\n").unwrap();
        run(&["add", "tracked"]);
        run(&["-c", "commit.gpgsign=false", "commit", "-m", "agent"]);
        let pushed = String::from_utf8(head()).unwrap().trim().to_owned();
        let loc = thegn_core::remote::GitLoc::Local(dir.path().to_path_buf());
        assert_eq!(
            observe_own_head(&loc, &admitted, || Some(pushed.clone())),
            Some(pushed.clone())
        );
        assert_eq!(
            observe_own_head(&loc, &admitted, || Some(admitted.clone())),
            None
        );
        assert_eq!(observe_own_head(&loc, &admitted, || None), None);
        assert_eq!(
            observe_own_head(&loc, &admitted, || Some("f".repeat(40))),
            None
        );
        let remote = thegn_core::remote::GitLoc::from_db(
            dir.path().to_str().unwrap(),
            Some(r#"{"host":"example.invalid","port":22,"path":"/repo"}"#),
        );
        assert!(matches!(&remote, thegn_core::remote::GitLoc::Remote { .. }));
        let called = Cell::new(false);
        assert_eq!(
            observe_own_head(&remote, &admitted, || {
                called.set(true);
                None
            }),
            None
        );
        assert!(!called.get());

        run(&["reset", "--hard", &admitted]);
        let called = Cell::new(false);
        assert_eq!(
            observe_own_head(&loc, &admitted, || {
                called.set(true);
                Some(pushed)
            }),
            None
        );
        assert!(!called.get());
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture runs isolated local git commands"
    )]
    fn git_fixture_repo(dir: &std::path::Path) {
        thegn_core::fsperm::restrict_dir_to_owner(dir).unwrap();
        let run = |args: &[&str]| {
            let status = thegn_core::util::git_cmd(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        run(&["-c", "commit.gpgsign=false", "init"]);
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("tracked"), "base\n").unwrap();
        run(&["add", "tracked"]);
        run(&["-c", "commit.gpgsign=false", "commit", "-m", "base"]);
    }

    #[test]
    fn record_own_head_is_fenced_to_the_live_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("lease.db")).unwrap();
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
        let request = PrAgentAdmit {
            key: &row.key,
            expected_attempts: 0,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: "head-a",
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: lease_until_ms(),
            owner_hint: Some("test"),
            ci_candidate: None,
        };
        let guard = PrAgentGuard::admit(&db, &request).unwrap().unwrap();
        assert!(guard.record_own_head(&db, "head-a", "head-b").unwrap());
        let lease = guard
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(guard.finish(&db, None).unwrap());
        assert!(!db.record_pr_agent_head(&lease, "head-b", "head-c").unwrap());
    }

    #[test]
    fn own_head_database_error_becomes_a_driver_warning() {
        let warning =
            own_head_record_warning(7, Err(anyhow::anyhow!("injected database failure"))).unwrap();
        assert!(warning.contains("could not be recorded under its lease"));
        assert!(warning.contains("injected database failure"));
    }
}
