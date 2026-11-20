//! Provider conformance suite (WS08-P1-08): one `suite` entry point run over
//! today's `RemoteProvider` and its optional traits, against the sprites fake
//! server, the Vercel `FakeApi`, and Fly and VPS fakes. The cases are
//! `create_records_before_request`, `destroy_missing_is_ok`,
//! `destroy_unrecorded_refused`, `inventory_contains_created`,
//! `run_captures_exit_and_output`, `exec_without_exit_fails`,
//! `stopped_run_is_retried_once`, `reads_never_resume`,
//! `inventory_partial_keeps_created`, `unfinished_listing_is_never_complete`,
//! `create_respects_cap`, `egress_never_widens`, `stop_resume_round_trip`,
//! `destroy_keeps_record_until_gone`, `destroy_is_idempotent`, and
//! `probe_report_shape`. WS08-P1-08 extends this file rather than replacing it:
//! add cases to `suite`, keep the names stable.
//!
//! A case is *skipped* (never failed) only when the [`Fixture`] says the op is
//! unbacked (no probe, no `foreign`, `supports_run == false`).
//! A new `Fixture` field is optional; each fixture test pins its `report.skipped`
//! list so a skip is always visible.

use std::future::Future;

use crate::provider::{InventoryReport, Provider};

/// A probe over the fake control plane the fixture runs against.
pub type Probe<T> = Box<dyn Fn() -> T + Send + Sync>;

/// Counts the DELETE requests a fake has seen for one name.
pub type DeleteCounter = Box<dyn Fn(&str) -> usize + Send + Sync>;

/// Arms (`true`) or disarms (`false`) the fake so a DELETE of the name is accepted (2xx)
/// while the sandbox stays present on every later read.
pub type HoldSwitch = Box<dyn Fn(&str, bool) + Send + Sync>;

/// Arms (`true`) or disarms (`false`) a fake-wide behaviour.
pub type Switch = Box<dyn Fn(bool) + Send + Sync>;

/// Makes the fake refuse the next `n` one-shot runs as "the session stopped"
/// (refused in the response head: nothing started); `0` disarms.
pub type RunRefusal = Box<dyn Fn(u32) + Send + Sync>;

/// Whether the network policy the fake last accepted for a name lets every
/// domain through; `None` when the fake accepted none.
pub type EgressProbe = Box<dyn Fn(&str) -> Option<bool> + Send + Sync>;

/// Whether this host's ownership record for a name is on disk.
pub type RecordProbe = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// What one provider run of the suite works against.
#[derive(Default)]
pub struct Fixture {
    /// The sandbox the provider's `create` yields (and the provider owns).
    pub created: String,
    /// A name no sandbox has.
    pub missing: String,
    /// A sandbox the provider can see but did not create, when the fixture has one.
    pub foreign: Option<String>,
    /// Whether the provider can run a command in a sandbox.
    pub supports_run: bool,
    /// The `(exit code, output substring)` the fake's exec answers with.
    pub run_expect: (i32, &'static str),
    /// Whether the ownership ledger record existed when the create request
    /// arrived (the fake records it); `None` when the fixture cannot tell.
    pub ledger_at_create: Option<Probe<Option<bool>>>,
    /// How many DELETE requests the fake has seen for a name.
    pub deletes_for: Option<Probe<DeleteCounter>>,
    /// How many wakes the fake has seen (Vercel resume requests, Sprites exec
    /// upgrades); `None` skips `reads_never_resume`.
    pub resumes: Option<Probe<usize>>,
    /// The provider's instance cap (`max_instances`) when the fixture is
    /// configured with one the suite can fill; `None` skips `create_respects_cap`.
    pub cap: Option<usize>,
    /// Whether the fake backs `suspend` + `resume` (Fly, Vercel); `false`
    /// skips `stop_resume_round_trip`.
    pub supports_stop: bool,
    /// `None` (with `recorded`) skips `destroy_keeps_record_until_gone`.
    pub hold_after_delete: Option<HoldSwitch>,
    /// `None` skips `destroy_keeps_record_until_gone`.
    pub recorded: Option<RecordProbe>,
    /// Backs `exec_without_exit_fails`; `None` skips it.
    pub drop_exit: Option<Switch>,
    /// Backs `stopped_run_is_retried_once`; `None` skips it.
    pub refuse_runs_stopped: Option<RunRefusal>,
    /// One-shot run requests the fake saw, refused or not; `None` skips
    /// `stopped_run_is_retried_once`.
    pub runs: Option<Probe<usize>>,
    /// Backs `unfinished_listing_is_never_complete`; `None` skips it.
    pub unfinished_listing: Option<Switch>,
    /// Backs `egress_never_widens`; `None` skips it.
    pub egress_allows_all: Option<EgressProbe>,
}

/// The outcome of one suite run, case by case.
#[derive(Debug, Default)]
pub struct Report {
    pub passed: Vec<&'static str>,
    pub failed: Vec<(&'static str, String)>,
    pub skipped: Vec<&'static str>,
}

impl Report {
    /// Panic with every failure, if any.
    pub fn assert_green(&self) {
        assert!(self.failed.is_empty(), "conformance failures: {self:#?}");
    }

    fn record(&mut self, case: &'static str, result: Result<(), String>) {
        match result {
            Ok(()) => self.passed.push(case),
            Err(why) => self.failed.push((case, why)),
        }
    }
}

fn ensure(cond: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if cond { Ok(()) } else { Err(why()) }
}

fn conformance_argv() -> [String; 3] {
    ["sh".into(), "-c".into(), "echo conformance".into()]
}

async fn case<F: Future<Output = Result<(), String>>>(
    report: &mut Report,
    name: &'static str,
    run: Option<F>,
) {
    match run {
        Some(run) => {
            let result = run.await;
            report.record(name, result);
        }
        None => report.skipped.push(name),
    }
}

/// Run every conformance case against `p`.
pub async fn suite(p: &Provider, fx: &Fixture) -> Report {
    let mut report = Report::default();

    case(
        &mut report,
        "create_records_before_request",
        fx.ledger_at_create.as_ref().map(|probe| async move {
            p.create()
                .await
                .map_err(|e| format!("create failed: {e:#}"))?;
            ensure(probe() == Some(true), || {
                "the ledger record was not on disk when the create request arrived".into()
            })
        }),
    )
    .await;

    case(
        &mut report,
        "destroy_missing_is_ok",
        Some(async {
            // Whether a DELETE goes out is the provider's business: a sandbox
            // this host recorded is deleted 404-tolerantly, an unrecorded one
            // is only probed. The contract is that teardown is idempotent.
            p.destroy(&fx.missing)
                .await
                .map_err(|e| format!("destroying a missing sandbox failed: {e:#}"))
        }),
    )
    .await;

    case(
        &mut report,
        "destroy_unrecorded_refused",
        fx.foreign.as_ref().map(|foreign| async move {
            let count = fx.deletes_for.as_ref().map(|probe| probe());
            ensure(p.destroy(foreign).await.is_err(), || {
                format!("destroy of unrecorded {foreign} succeeded")
            })?;
            let after = count.as_ref().map_or(0, |c| c(foreign));
            ensure(after == 0, || {
                format!("a DELETE for unrecorded {foreign} was sent")
            })
        }),
    )
    .await;

    case(
        &mut report,
        "inventory_contains_created",
        Some(async {
            let ids = match p.inventory().await {
                Ok(InventoryReport::Complete(rows)) => rows,
                Ok(InventoryReport::Partial { rows, .. }) => rows,
                Err(e) => return Err(format!("inventory failed: {e:#}")),
            };
            ensure(ids.iter().any(|row| row.id == fx.created), || {
                format!("inventory lacks {}: {ids:?}", fx.created)
            })
        }),
    )
    .await;

    case(
        &mut report,
        "run_captures_exit_and_output",
        fx.supports_run.then_some(async {
            let argv = conformance_argv();
            let (code, out) = p
                .run_exec(&fx.created, &argv, None, &[])
                .await
                .map_err(|e| format!("run_exec failed: {e:#}"))?;
            let (want_code, want_out) = fx.run_expect;
            ensure(code == want_code && out.contains(want_out), || {
                format!("got ({code}, {out:?}), want ({want_code}, ..{want_out:?}..)")
            })
        }),
    )
    .await;

    case(
        &mut report,
        "exec_without_exit_fails",
        fx.drop_exit.as_ref().map(|arm| async move {
            arm(true);
            let result = p
                .run_exec(&fx.created, &conformance_argv(), None, &[])
                .await;
            arm(false);
            match result {
                Ok((code, out)) => Err(format!(
                    "an exec stream with no exit status returned ({code}, {out:?})"
                )),
                Err(_) => Ok(()),
            }
        }),
    )
    .await;

    case(
        &mut report,
        "stopped_run_is_retried_once",
        fx.refuse_runs_stopped
            .as_ref()
            .zip(fx.runs.as_ref())
            .map(|(refuse, runs)| async move {
                refuse(1);
                let before = runs();
                let first_result = p
                    .run_exec(&fx.created, &conformance_argv(), None, &[])
                    .await;
                let first = runs().saturating_sub(before);

                refuse(2);
                let before = runs();
                let second_result = p
                    .run_exec(&fx.created, &conformance_argv(), None, &[])
                    .await;
                let second = runs().saturating_sub(before);

                refuse(0);
                ensure(first_result.is_ok() && first == 2, || {
                    "a run refused once because the session stopped was not retried exactly once"
                        .into()
                })?;
                ensure(second_result.is_err() && second == 2, || {
                    "a second stop refusal was retried past the one allowed retry".into()
                })
            }),
    )
    .await;

    case(
        &mut report,
        "reads_never_resume",
        fx.resumes.as_ref().map(|probe| async move {
            let before = probe();
            p.list().await.map_err(|e| format!("list failed: {e:#}"))?;
            p.inventory()
                .await
                .map_err(|e| format!("inventory failed: {e:#}"))?;
            let after = probe();
            ensure(after == before, || {
                format!("reads resumed a sandbox ({before} -> {after})")
            })
        }),
    )
    .await;

    case(
        &mut report,
        "inventory_partial_keeps_created",
        Some(async {
            // A Partial listing must still carry every row it did read: the
            // created sandbox may never vanish from a degraded report.
            match p.inventory().await {
                Ok(InventoryReport::Complete(_)) => Ok(()),
                Ok(InventoryReport::Partial { rows, .. }) => {
                    ensure(rows.iter().any(|row| row.id == fx.created), || {
                        format!("partial inventory dropped {}: {rows:?}", fx.created)
                    })
                }
                Err(e) => Err(format!("inventory failed: {e:#}")),
            }
        }),
    )
    .await;

    case(
        &mut report,
        "unfinished_listing_is_never_complete",
        fx.unfinished_listing.as_ref().map(|arm| async move {
            arm(true);
            let inventory = p.inventory().await;
            arm(true);
            let list = p.list().await;
            arm(false);

            ensure(
                !matches!(&inventory, Ok(InventoryReport::Complete(_))),
                || "an unfinished listing was reported Complete".into(),
            )?;
            if let Ok(InventoryReport::Partial { rows, .. }) = &inventory {
                ensure(rows.iter().any(|row| row.id == fx.created), || {
                    format!("partial inventory dropped {}: {rows:?}", fx.created)
                })?;
            }
            ensure(list.is_err(), || {
                "list() accepted an unfinished listing".into()
            })
        }),
    )
    .await;

    case(
        &mut report,
        "create_respects_cap",
        fx.cap.map(|cap| async move {
            // Fill the cap, then the cap+1 create must be refused. Creates the
            // fake already satisfied count toward the cap (idempotent by name).
            let mut refused = false;
            for _ in 0..=cap {
                if p.create().await.is_err() {
                    refused = true;
                    break;
                }
            }
            ensure(refused, || {
                format!("create {} succeeded past the cap of {cap}", cap + 1)
            })
        }),
    )
    .await;

    case(
        &mut report,
        "egress_never_widens",
        fx.egress_allows_all.as_ref().map(|probe| async move {
            let allow = ["a.example".to_string()];
            let shadowed = p.set_network_policy(&fx.created, &allow, &allow).await;
            if shadowed.is_ok() {
                ensure(probe(&fx.created) == Some(false), || {
                    "an allow list emptied by blocks was applied as allow-all".into()
                })?;
            }
            let applied = p.set_network_policy(&fx.created, &allow, &[]).await;
            ensure(applied.is_ok(), || {
                "a non-empty allow list was refused".into()
            })?;
            ensure(probe(&fx.created) == Some(false), || {
                "an allow list was applied as allow-all".into()
            })
        }),
    )
    .await;

    case(
        &mut report,
        "stop_resume_round_trip",
        fx.supports_stop.then_some(async {
            p.suspend(&fx.created)
                .await
                .map_err(|e| format!("suspend failed: {e:#}"))?;
            p.suspend(&fx.created)
                .await
                .map_err(|e| format!("second suspend not idempotent: {e:#}"))?;
            p.resume(&fx.created)
                .await
                .map_err(|e| format!("resume failed: {e:#}"))
        }),
    )
    .await;

    case(
        &mut report,
        "destroy_keeps_record_until_gone",
        fx.hold_after_delete.as_ref().zip(fx.recorded.as_ref()).map(
            |(hold, recorded)| async move {
                ensure(recorded(&fx.created), || {
                    "the ownership record was not present before destroy".into()
                })?;
                hold(&fx.created, true);
                let result = p.destroy(&fx.created).await;
                hold(&fx.created, false);
                ensure(result.is_err(), || {
                    "destroy reported success while the sandbox was still present".into()
                })?;
                ensure(recorded(&fx.created), || {
                    "the ownership record was dropped before the sandbox was proven gone".into()
                })
            },
        ),
    )
    .await;

    // Last: it removes the sandbox the cases above work against.
    case(
        &mut report,
        "destroy_is_idempotent",
        Some(async {
            // Idempotence is the contract, not the request count: a provider
            // may re-send a 404-tolerant DELETE for a sandbox it recorded.
            p.destroy(&fx.created)
                .await
                .map_err(|e| format!("first destroy failed: {e:#}"))?;
            p.destroy(&fx.created)
                .await
                .map_err(|e| format!("second destroy failed: {e:#}"))?;
            let ids = match p.inventory().await {
                Ok(InventoryReport::Complete(rows)) => rows,
                Ok(InventoryReport::Partial { rows, .. }) => rows,
                Err(e) => return Err(format!("inventory failed: {e:#}")),
            };
            ensure(!ids.iter().any(|row| row.id == fx.created), || {
                format!("destroyed {} still in inventory: {ids:?}", fx.created)
            })
        }),
    )
    .await;

    let shape = std::panic::catch_unwind(|| {
        let mut cfg = thegn_core::config::Config::default();
        cfg.host_discovery.tailnet.enabled = false;
        crate::conformance::assert_report_invariants(&crate::seam::registry::probes(&cfg));
    });
    report.record(
        "probe_report_shape",
        shape.map_err(|_| "assert_report_invariants panicked".to_string()),
    );

    report
}
