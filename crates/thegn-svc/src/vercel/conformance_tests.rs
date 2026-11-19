//! The provider conformance suite run against the Vercel `FakeApi` (b160
//! lane 8; first slice of WS08-P1-08, see `crate::provider_conformance`).

use std::sync::Arc;

use super::provider_tests::{FakeApi, NAME, provider, tagged, view};
use super::*;
use crate::provider::Provider;
use crate::provider_conformance::{Fixture, suite};

#[tokio::test]
async fn vercel_passes_the_conformance_suite() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi {
        stateful: true,
        ..FakeApi::default()
    });
    *fake.create.lock().unwrap() = Some(Ok(tagged(NAME, &controller)));
    *fake.ledger_probe.lock().unwrap() = Some(ledger.path().join(format!("{NAME}.json")));
    // A sandbox this controller did not create: no thegn tag at all.
    fake.by_name.lock().unwrap().insert(
        "tg-vercel-foreign".into(),
        view("tg-vercel-foreign", SessionStatus::Running, Some("sess_f")),
    );

    let mut vp = provider(&fake, ledger.path(), 1);
    vp.destroy_budget = std::time::Duration::from_millis(200);
    vp.poll = std::time::Duration::from_millis(20);
    let p = Provider::Vercel(vp);
    let (seen, counts, resumes) = (fake.clone(), fake.clone(), fake.clone());
    let hold_fake = fake.clone();
    let ledger_path = ledger.path().to_path_buf();
    let fx = Fixture {
        created: NAME.into(),
        missing: "tg-vercel-missing".into(),
        foreign: Some("tg-vercel-foreign".into()),
        supports_run: true,
        run_expect: (0, "ok"),
        ledger_at_create: Some(Box::new(move || *seen.ledger_seen.lock().unwrap())),
        deletes_for: Some(Box::new(move || {
            let fake = counts.clone();
            Box::new(move |name: &str| {
                let want = format!("delete {name}");
                fake.calls().iter().filter(|c| **c == want).count()
            })
        })),
        resumes: Some(Box::new(move || resumes.count("resume"))),
        // The first suite case records the created sandbox in the ledger, so
        // max_instances = 1 makes the later cap+1 create refuse before HTTP.
        // The untagged foreign sandbox is filtered out of the ledger count.
        cap: Some(1),
        supports_stop: true,
        hold_after_delete: Some(Box::new(move |name, hold| {
            let mut gets = hold_fake.gets.lock().unwrap();
            if hold {
                gets.push_back(Some(view(name, SessionStatus::Running, Some("sess_h"))));
            } else {
                gets.clear();
            }
        })),
        recorded: Some(Box::new(move |name| {
            ledger_path.join(format!("{name}.json")).exists()
        })),
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert!(report.skipped.is_empty(), "{report:?}");
    assert!(
        report.passed.contains(&"destroy_keeps_record_until_gone"),
        "{report:?}"
    );
    assert!(fake.count("stop") >= 1, "{report:?}");
    assert_eq!(fake.count("resume"), 1, "{report:?}");
}
