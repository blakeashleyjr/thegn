//! Provider conformance suite (first slice of WS08-P1-08): one `suite` entry
//! point run over today's `RemoteProvider` and its optional traits, against the
//! sprites fake server, the Vercel `FakeApi`, and (WS08-P1-08 slice 2) Fly and VPS fakes. WS08-P1-08 extends this file
//! rather than replacing it: add cases to `suite`, keep the names stable.
//!
//! A case is *skipped* (never failed) only when the [`Fixture`] says the op is
//! unbacked (no probe, no `foreign`, `supports_run == false`).

use std::future::Future;

use crate::provider::{InventoryReport, Provider};

/// A probe over the fake control plane the fixture runs against.
pub type Probe<T> = Box<dyn Fn() -> T + Send + Sync>;

/// Counts the DELETE requests a fake has seen for one name.
pub type DeleteCounter = Box<dyn Fn(&str) -> usize + Send + Sync>;

/// What one provider run of the suite works against.
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
    /// How many resume requests the fake has seen (none for sprites).
    pub resumes: Option<Probe<usize>>,
    /// The provider's instance cap (`max_instances`) when the fixture is
    /// configured with one the suite can fill; `None` skips `create_respects_cap`.
    pub cap: Option<usize>,
    /// Whether the fake backs `suspend` + `resume` (Fly, Vercel); `false`
    /// skips `stop_resume_round_trip`.
    pub supports_stop: bool,
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
            let argv = [
                "sh".to_string(),
                "-c".to_string(),
                "echo conformance".into(),
            ];
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
