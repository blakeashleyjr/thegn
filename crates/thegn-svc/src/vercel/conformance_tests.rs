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
    let fake = Arc::new(FakeApi::default());
    *fake.create.lock().unwrap() = Some(Ok(tagged(NAME, &controller)));
    *fake.ledger_probe.lock().unwrap() = Some(ledger.path().join(format!("{NAME}.json")));
    // A sandbox this controller did not create: no thegn tag at all.
    fake.by_name.lock().unwrap().insert(
        "tg-vercel-foreign".into(),
        view("tg-vercel-foreign", SessionStatus::Running, Some("sess_f")),
    );

    let p = Provider::Vercel(provider(&fake, ledger.path(), 5));
    let (seen, counts, resumes) = (fake.clone(), fake.clone(), fake.clone());
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
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert!(report.skipped.is_empty(), "{report:?}");
}
