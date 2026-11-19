//! Worktree-close/delete teardown for managed-provider sandboxes (extracted
//! from the ratchet-pinned `agent.rs`; re-exported from `crate::agent` so call
//! sites are unchanged). Both are best-effort and run off the event loop.

use crate::agent::{block_on_provider, provider_sandbox_name};
use crate::provider_factory::provider_for_named;
use std::time::{Duration, Instant};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::WorkspaceStore;

/// Drop a destroyed spare's ledger row only after the provider destroy
/// succeeded; a failure keeps the row (ownership).
fn after_destroy(
    outcome: Result<(), String>,
    disposition: crate::lifecycle::DeleteRecycle,
    forget: impl FnOnce(),
) -> Result<(), String> {
    outcome?;
    if disposition == (crate::lifecycle::DeleteRecycle::Destroy { pool_spare: true }) {
        forget();
    }
    Ok(())
}

/// Suspend-on-close (8-E): for a provider env with `auto_checkpoint`, snapshot the
/// sandbox when the worktree closes (fast resume next open). Called from the
/// fire-and-forget close thread, which has only the path — so it loads config +
/// resolves the env itself. Best-effort + off-loop; checkpoints-capable only.
pub fn checkpoint_on_close(worktree: &str) {
    let cfg = crate::config_startup::cleanup_config();
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            crate::close_teardown::warn_visible(&format!(
                "auto-checkpoint on close skipped: the environment selection could not be read: {error:#}"
            ));
            return;
        }
    };
    let repo_root = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .unwrap_or_default();
    let env_name = match checkpoint_selection(db.effective_env_checked(worktree, &repo_root)) {
        Ok(Some(name)) => name,
        Ok(None) => return,
        Err(message) => {
            crate::close_teardown::warn_visible(&message);
            return;
        }
    };
    let Some(env) = cfg.env.get(&env_name) else {
        return;
    };
    let pc = &env.provider;
    if !pc.auto_checkpoint {
        return;
    }
    // Per-worktree id from the single source of truth (resolved placement).
    let Some(name) = provider_sandbox_name(&cfg, worktree, &env_name).filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(provider) = provider_for_named(pc, &name) else {
        return;
    };
    if !provider.caps().checkpoints {
        return;
    }
    match block_on_provider(|| async { provider.checkpoint(&name, Some("auto-close")).await }) {
        Ok(id) => thegn_core::msg::info(&format!("checkpointed {name} on close: {id}")),
        Err(error) => {
            crate::close_teardown::warn_visible(&checkpoint_failed_message(&name, &error))
        }
    }
}

fn checkpoint_failed_message(name: &str, error: &dyn std::fmt::Display) -> String {
    format!("auto-checkpoint on close failed for {name}: {error:#}")
}

/// The env to checkpoint on close: `Ok(None)` = nothing selected, `Err` = unreadable (skip with a warning).
fn checkpoint_selection(read: anyhow::Result<Option<String>>) -> Result<Option<String>, String> {
    read.map_err(|error| format!(
        "auto-checkpoint on close skipped: the environment selection could not be read: {error:#}"
    ))
}

/// How long a sent destroy may take to disappear from a complete inventory.
const DESTROY_VERIFY_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between inventory polls while confirming a destroy.
const DESTROY_VERIFY_POLL: Duration = Duration::from_secs(3);

/// What a worktree teardown did to its provider sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Teardown {
    /// No provider sandbox is bound to this worktree under this env.
    NoSandbox,
    /// A claimed pool spare was reset into the pool (it still exists, by design).
    Recycled { name: String },
    /// Destroyed, and a complete provider inventory no longer lists it.
    Destroyed { name: String },
}

/// The two provider calls a confirmed destroy needs (a seam over
/// [`thegn_svc::provider::Provider`] so the decision is testable offline).
trait SandboxOps {
    fn destroy(&mut self, name: &str) -> Result<(), String>;
    fn inventory(&mut self) -> anyhow::Result<thegn_svc::provider::InventoryReport>;
}

struct LiveOps(thegn_svc::provider::Provider);

impl SandboxOps for LiveOps {
    fn destroy(&mut self, name: &str) -> Result<(), String> {
        let provider = &self.0;
        block_on_provider(|| async { provider.destroy(name).await }).map_err(|e| format!("{e:#}"))
    }

    fn inventory(&mut self) -> anyhow::Result<thegn_svc::provider::InventoryReport> {
        let provider = &self.0;
        block_on_provider(|| async { provider.inventory().await })
    }
}

/// Destroy a provider sandbox using an already-resolved configuration. The
/// lifecycle transaction calls this before deleting the worktree row, so the
/// selected environment and provider identity cannot be lost to cache pruning.
///
/// Blocking (polls the provider inventory for up to a minute): call it only off
/// the event loop. A destroy counts only once a COMPLETE inventory no longer
/// lists the sandbox; until then the spare row and provisioned marker are kept.
pub fn destroy_provider_sandbox_with(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
) -> Result<Teardown, String> {
    let Some(env) = cfg.env.get(env_name) else {
        return Ok(Teardown::NoSandbox);
    };
    let pc = &env.provider;
    let name = match crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, env_name)
    {
        Ok(name) => name,
        Err(error) => {
            let message = format!("resolve the worktree's provider sandbox: {error:#}");
            return Err(message);
        }
    };
    let Some(name) = name.filter(|s| !s.is_empty()) else {
        return Ok(Teardown::NoSandbox);
    };
    // Revoke before recycle/destroy. If SQLite is unavailable, fail closed and
    // leave the sandbox intact so the lifecycle can retry rather than orphaning
    // an unrevoked return credential with no remaining owner metadata.
    crate::remote_enqueue_auth::revoke_for_sandbox(pc, &name, Some(worktree))
        .map_err(|error| format!("revoke route-to-host credential: {error:#}"))?;
    // A CLAIMED pool spare with a fresh provisioned-base checkpoint is RECYCLED
    // back into the pool (restore-in-place, row → `ready`) instead of destroyed.
    let disposition = crate::lifecycle::recycle_claimed_on_delete(cfg, env_name, &name);
    if disposition == crate::lifecycle::DeleteRecycle::Recycled {
        thegn_core::msg::info(&format!("recycled spare {name} into the pool on delete"));
        return Ok(Teardown::Recycled { name });
    }
    let mut ops = provider_for_named(pc, &name).map(LiveOps);
    let outcome = destroy_confirmed(
        ops.as_mut().map(|ops| ops as &mut dyn SandboxOps),
        pc.provider.trim(),
        worktree,
        &name,
        &mut || Instant::now() + DESTROY_VERIFY_TIMEOUT,
        &mut std::thread::sleep,
    );
    after_destroy(outcome, disposition, || {
        crate::lifecycle::forget_destroyed_spare(&name)
    })?;
    thegn_core::msg::info(&format!("destroyed sandbox {name} on worktree delete"));
    // Drop the local provisioned marker so a later sandbox reusing this id isn't
    // treated as provisioned (the attach gate would skip re-provisioning).
    crate::provider_workdir::clear_provisioned(&name);
    Ok(Teardown::Destroyed { name })
}

/// Send the destroy and confirm it against a complete inventory. `None` ops =
/// the provider is not usable (token unset): nothing is sent. Seam.
fn destroy_confirmed(
    ops: Option<&mut dyn SandboxOps>,
    provider: &str,
    worktree: &str,
    name: &str,
    deadline_after_send: &mut dyn FnMut() -> Instant,
    sleep: &mut dyn FnMut(Duration),
) -> Result<(), String> {
    let Some(ops) = ops else {
        return Err(format!(
            "{provider} is not usable (token unset?): sandbox {name} was NOT destroyed and may \
             still bill; set the token and retry, or `thegn env set --worktree {worktree} <local env>` \
             to detach it (`thegn env reap --provider {provider}` lists it)"
        ));
    };
    ops.destroy(name)
        .map_err(|error| format!("destroy sandbox {name}: {error}"))?;
    // The deadline starts once the destroy was accepted, not before the call.
    let deadline = deadline_after_send();
    wait_absent(&mut || ops.inventory(), name, deadline, sleep).map_err(|why| {
        format!(
            "destroy of sandbox {name} was sent but could not be confirmed ({why}); it may still \
             bill — retry, or `thegn env reap --provider {provider}`"
        )
    })
}

/// Poll until a COMPLETE inventory lacks `id`. A partial inventory, a list error or a
/// still-listed id retries until `deadline`; then Err naming the last reason. Pure seam.
fn wait_absent(
    list: &mut dyn FnMut() -> anyhow::Result<thegn_svc::provider::InventoryReport>,
    id: &str,
    deadline: Instant,
    sleep: &mut dyn FnMut(Duration),
) -> Result<(), String> {
    wait_absent_at(list, id, deadline, &mut Instant::now, sleep)
}

fn wait_absent_at(
    list: &mut dyn FnMut() -> anyhow::Result<thegn_svc::provider::InventoryReport>,
    id: &str,
    deadline: Instant,
    now: &mut dyn FnMut() -> Instant,
    sleep: &mut dyn FnMut(Duration),
) -> Result<(), String> {
    use thegn_svc::provider::InventoryReport;
    loop {
        let why = match list() {
            Ok(report) => match crate::provider_git::sandbox_in_inventory(&report, id) {
                Some(false) => return Ok(()),
                Some(true) => format!("the provider still lists {id}"),
                None => match &report {
                    InventoryReport::Partial { reason, .. } => {
                        format!("the provider inventory was partial: {reason}")
                    }
                    InventoryReport::Complete(_) => "the provider inventory was incomplete".into(),
                },
            },
            Err(error) => format!("listing the provider failed: {error:#}"),
        };
        let at = now();
        if at >= deadline {
            return Err(why);
        }
        sleep(DESTROY_VERIFY_POLL.min(deadline - at));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::DeleteRecycle;

    #[test]
    fn pool_delete_destroy_failure_keeps_the_row() {
        let mut called = false;
        let result = after_destroy(
            Err("boom".into()),
            DeleteRecycle::Destroy { pool_spare: true },
            || called = true,
        );

        assert_eq!(result, Err("boom".into()));
        assert!(!called);
    }

    #[test]
    fn pool_delete_destroy_success_drops_the_row_once() {
        let mut calls = 0;
        let result = after_destroy(Ok(()), DeleteRecycle::Destroy { pool_spare: true }, || {
            calls += 1
        });

        assert_eq!(result, Ok(()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn pool_delete_success_without_spare_row_skips_forget() {
        let mut called = false;
        let result = after_destroy(Ok(()), DeleteRecycle::Destroy { pool_spare: false }, || {
            called = true
        });

        assert_eq!(result, Ok(()));
        assert!(!called);
    }

    use thegn_svc::provider::{InventoryReport, SandboxInfo};

    type ListResult = anyhow::Result<InventoryReport>;

    /// A fake clock: `sleep` advances it, `now` reads it. No real sleep.
    struct FakeClock {
        start: Instant,
        elapsed: std::cell::Cell<Duration>,
        sleeps: std::cell::Cell<usize>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                elapsed: std::cell::Cell::new(Duration::ZERO),
                sleeps: std::cell::Cell::new(0),
            }
        }
        fn now(&self) -> Instant {
            self.start + self.elapsed.get()
        }
        fn sleep(&self, d: Duration) {
            self.elapsed.set(self.elapsed.get() + d);
            self.sleeps.set(self.sleeps.get() + 1);
        }
    }

    fn run_wait(results: &mut dyn FnMut() -> ListResult, clock: &FakeClock) -> Result<(), String> {
        let deadline = clock.now() + DESTROY_VERIFY_TIMEOUT;
        wait_absent_at(results, "tg-a", deadline, &mut || clock.now(), &mut |d| {
            clock.sleep(d)
        })
    }

    #[test]
    fn wait_absent_complete_without_id_is_ok_on_the_first_poll() {
        let clock = FakeClock::new();
        let mut polls = 0;
        let result = run_wait(
            &mut || {
                polls += 1;
                Ok(InventoryReport::Complete(vec![SandboxInfo::id_only(
                    "tg-other",
                )]))
            },
            &clock,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(polls, 1);
        assert_eq!(clock.sleeps.get(), 0);
    }

    #[test]
    fn wait_absent_partial_without_id_retries_then_fails_at_the_deadline() {
        let clock = FakeClock::new();
        let mut polls = 0;
        let result = run_wait(
            &mut || {
                polls += 1;
                Ok(InventoryReport::Partial {
                    rows: vec![],
                    reason: "page 2 failed".into(),
                })
            },
            &clock,
        );
        let error = result.unwrap_err();
        assert!(error.contains("partial"), "{error}");
        assert!(error.contains("page 2 failed"), "{error}");
        assert!(polls > 1, "a partial listing must be retried");
        assert!(clock.elapsed.get() >= DESTROY_VERIFY_TIMEOUT);
    }

    #[test]
    fn wait_absent_still_listed_fails_naming_it() {
        let clock = FakeClock::new();
        let error = run_wait(
            &mut || {
                Ok(InventoryReport::Complete(vec![SandboxInfo::id_only(
                    "tg-a",
                )]))
            },
            &clock,
        )
        .unwrap_err();
        assert!(error.contains("still lists tg-a"), "{error}");
    }

    #[test]
    fn wait_absent_list_error_fails_with_the_error_text() {
        let clock = FakeClock::new();
        let error = run_wait(&mut || Err(anyhow::anyhow!("401 unauthorized")), &clock).unwrap_err();
        assert!(error.contains("401 unauthorized"), "{error}");
    }

    #[test]
    fn wait_absent_succeeds_once_a_later_poll_is_complete() {
        let clock = FakeClock::new();
        let mut polls = 0;
        let result = run_wait(
            &mut || {
                polls += 1;
                if polls < 3 {
                    Ok(InventoryReport::Complete(vec![SandboxInfo::id_only(
                        "tg-a",
                    )]))
                } else {
                    Ok(InventoryReport::Complete(vec![]))
                }
            },
            &clock,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(polls, 3);
        assert_eq!(clock.sleeps.get(), 2);
    }

    #[derive(Default)]
    struct FakeOps {
        destroys: usize,
        listed: bool,
    }

    impl SandboxOps for FakeOps {
        fn destroy(&mut self, _name: &str) -> Result<(), String> {
            self.destroys += 1;
            Ok(())
        }
        fn inventory(&mut self) -> anyhow::Result<InventoryReport> {
            let rows = if self.listed {
                vec![SandboxInfo::id_only("tg-a")]
            } else {
                vec![]
            };
            Ok(InventoryReport::Complete(rows))
        }
    }

    fn confirm(ops: Option<&mut dyn SandboxOps>, clock: &FakeClock) -> Result<(), String> {
        destroy_confirmed(
            ops,
            "sprites",
            "/wt/a",
            "tg-a",
            &mut || clock.now() + DESTROY_VERIFY_TIMEOUT,
            &mut |d| clock.sleep(d),
        )
    }

    #[test]
    fn destroy_without_a_usable_provider_refuses_and_sends_nothing() {
        let clock = FakeClock::new();
        let ops = FakeOps::default();
        let error = confirm(None, &clock).unwrap_err();
        assert!(error.contains("NOT destroyed"), "{error}");
        assert!(error.contains("tg-a"), "{error}");
        assert!(error.contains("thegn env set --worktree /wt/a"), "{error}");
        assert!(
            error.contains("thegn env reap --provider sprites"),
            "{error}"
        );
        assert_eq!(ops.destroys, 0);
    }

    #[test]
    fn destroy_confirmed_needs_the_sandbox_gone_from_the_inventory() {
        let clock = FakeClock::new();
        let mut gone = FakeOps::default();
        assert_eq!(confirm(Some(&mut gone), &clock), Ok(()));
        assert_eq!(gone.destroys, 1);

        let mut lingering = FakeOps {
            listed: true,
            ..FakeOps::default()
        };
        let error = confirm(Some(&mut lingering), &clock).unwrap_err();
        assert_eq!(lingering.destroys, 1);
        assert!(error.contains("could not be confirmed"), "{error}");
        assert!(error.contains("still lists tg-a"), "{error}");
        assert!(
            error.contains("thegn env reap --provider sprites"),
            "{error}"
        );
    }

    #[test]
    fn unconfirmed_destroy_keeps_the_spare_row() {
        let mut called = false;
        let result = after_destroy(
            Err("could not be confirmed".into()),
            DeleteRecycle::Destroy { pool_spare: true },
            || called = true,
        );
        assert!(result.is_err());
        assert!(!called);
    }

    #[test]
    fn destroy_for_an_unconfigured_env_is_no_sandbox() {
        let cfg = Config::default();
        assert_eq!(
            destroy_provider_sandbox_with(&cfg, "/wt/a", "not-configured"),
            Ok(Teardown::NoSandbox)
        );
    }

    #[test]
    fn checkpoint_selection_skips_an_unreadable_selection() {
        let Err(message) = checkpoint_selection(Err(anyhow::anyhow!("unavailable"))) else {
            panic!("unreadable selection must skip checkpoint");
        };

        assert!(message.starts_with("auto-checkpoint on close skipped"));
    }

    #[test]
    fn checkpoint_selection_passes_a_read_through() {
        assert_eq!(
            checkpoint_selection(Ok(Some("box".into()))),
            Ok(Some("box".into()))
        );
        assert_eq!(checkpoint_selection(Ok(None)), Ok(None));
    }

    #[test]
    fn checkpoint_failed_message_names_the_sandbox_and_cause() {
        let message = checkpoint_failed_message("sbx", &"provider unavailable");
        assert!(message.contains("auto-checkpoint on close failed for sbx"));
        assert!(message.contains("provider unavailable"));

        let error = anyhow::anyhow!("provider unavailable").context("checkpoint request failed");
        let message = checkpoint_failed_message("sbx", &error);
        assert!(message.contains("checkpoint request failed"));
        assert!(message.contains("provider unavailable"));
    }
}
