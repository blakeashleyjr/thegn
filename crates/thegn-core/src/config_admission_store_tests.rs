use super::*;
use crate::config::{Config, MapEnv, PathExpansionContext};
use crate::config_admission::{AdmissionInputs, SourceInput, admit};
use crate::host_definition_snapshot::HostDefinitionsSnapshot;

fn candidate(body: &str) -> AdmittedConfig {
    let env = MapEnv::default();
    let hosts = HostDefinitionsSnapshot::empty(crate::db::SCHEMA_VERSION);
    let paths = PathExpansionContext::from_home("/home/test".into());
    admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, body.as_bytes()),
        profile: None,
        env: &env,
        overrides: &[],
        hosts: &hosts,
        paths: &paths,
    })
    .expect("valid candidate")
}

#[test]
fn empty_store_authorizes_nothing() {
    let store = AdmissionStore::new();
    assert_eq!(store.health(), StoreHealth::Empty);
    assert!(store.display().is_none());
    let unpublished = candidate("branch_prefix = \"a/\"\n");
    assert_eq!(unpublished.revision().generation, 0);
    assert_eq!(
        store.authorize(&unpublished.revision()).unwrap_err().reason,
        StaleConfigReason::NoSnapshot
    );
}

#[test]
fn publication_allocates_generations_and_supersedes_old_revisions() {
    let store = AdmissionStore::new();
    let first = store
        .publish(candidate("branch_prefix = \"a/\"\n"), 0)
        .unwrap();
    assert_eq!(first.revision().generation, 1);
    assert!(store.authorize(&first.revision()).is_ok());

    let second = store
        .publish(candidate("branch_prefix = \"b/\"\n"), 1)
        .unwrap();
    assert_eq!(second.revision().generation, 2);
    assert_eq!(
        store.authorize(&first.revision()).unwrap_err().reason,
        StaleConfigReason::Superseded
    );
    assert_eq!(
        store
            .authorize(&second.revision())
            .unwrap()
            .config()
            .branch_prefix,
        "b/"
    );

    // A publisher that captured against generation 1 loses the CAS.
    let stale = store.publish(candidate("branch_prefix = \"c/\"\n"), 1);
    assert_eq!(stale.unwrap_err().reason, StaleConfigReason::Superseded);
    assert_eq!(store.display().unwrap().config().branch_prefix, "b/");
}

#[test]
fn identical_content_republished_is_still_a_new_generation() {
    let store = AdmissionStore::new();
    let first = store
        .publish(candidate("branch_prefix = \"a/\"\n"), 0)
        .unwrap();
    let again = store
        .publish(candidate("branch_prefix = \"a/\"\n"), 1)
        .unwrap();
    assert_ne!(first.revision(), again.revision());
    assert!(store.authorize(&first.revision()).is_err());
}

#[test]
fn a_forged_generation_on_an_unpublished_candidate_is_refused() {
    let store = AdmissionStore::new();
    store
        .publish(candidate("branch_prefix = \"a/\"\n"), 0)
        .unwrap();
    let mut forged = candidate("branch_prefix = \"evil/\"\n").revision();
    forged.generation = 1;
    assert_eq!(
        store.authorize(&forged).unwrap_err().reason,
        StaleConfigReason::Superseded
    );
}

#[test]
fn failed_reload_keeps_last_good_for_display_only_and_coalesces() {
    let store = AdmissionStore::new();
    let good = store
        .publish(candidate("branch_prefix = \"a/\"\n"), 0)
        .unwrap();
    assert_eq!(
        store.record_failure(ConfigAdmissionError::ParseInvalid, 7),
        FailureReport::First
    );
    assert_eq!(
        store.record_failure(ConfigAdmissionError::ParseInvalid, 7),
        FailureReport::Coalesced
    );
    assert_eq!(
        store.health(),
        StoreHealth::Degraded {
            generation: 1,
            error: ConfigAdmissionError::ParseInvalid,
            failures: 2,
        }
    );
    assert_eq!(store.display().unwrap().revision(), good.revision());
    assert_eq!(
        store.authorize(&good.revision()).unwrap_err().reason,
        StaleConfigReason::Degraded
    );
    assert_eq!(
        store.current().unwrap_err().reason,
        StaleConfigReason::Degraded
    );
    // A different failure is new information.
    assert_eq!(
        store.record_failure(ConfigAdmissionError::SchemaInvalid, 7),
        FailureReport::First
    );
    // …and so is a different problem of the same category.
    assert_eq!(
        store.record_failure(ConfigAdmissionError::SchemaInvalid, 8),
        FailureReport::First
    );
    // A successful publication clears the degradation.
    let next = store
        .publish(candidate("branch_prefix = \"b/\"\n"), 1)
        .unwrap();
    assert_eq!(store.health(), StoreHealth::Current { generation: 2 });
    assert!(store.authorize(&next.revision()).is_ok());
}

#[test]
fn failure_before_any_publication_has_nothing_to_display() {
    let store = AdmissionStore::new();
    store.record_failure(ConfigAdmissionError::Unreadable, 0);
    assert!(store.display().is_none());
    assert!(matches!(
        store.health(),
        StoreHealth::Degraded { generation: 0, .. }
    ));
    let first = store
        .publish(candidate("branch_prefix = \"a/\"\n"), 0)
        .unwrap();
    assert_eq!(first.revision().generation, 1);
}

#[test]
fn generation_exhaustion_is_a_typed_refusal() {
    let store = AdmissionStore::new();
    *store.state.lock().unwrap() = Some(State {
        current: Some(Arc::new(
            candidate("branch_prefix = \"max/\"\n").with_generation(u64::MAX),
        )),
        degraded: None,
    });
    let error = store
        .publish(candidate("branch_prefix = \"next/\"\n"), u64::MAX)
        .unwrap_err();
    assert_eq!(error.reason, StaleConfigReason::GenerationExhausted);
    assert_eq!(store.display().unwrap().revision().generation, u64::MAX);
}

#[test]
fn concurrent_publishers_against_one_generation_have_exactly_one_winner() {
    let store = Arc::new(AdmissionStore::new());
    store
        .publish(candidate("branch_prefix = \"a/\"\n"), 0)
        .unwrap();
    let handles: Vec<_> = (0..8)
        .map(|index| {
            let store = Arc::clone(&store);
            let next = candidate(&format!("branch_prefix = \"p{index}/\"\n"));
            std::thread::spawn(move || store.publish(next, 1).is_ok())
        })
        .collect();
    let winners = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
    assert_eq!(store.health(), StoreHealth::Current { generation: 2 });
}

#[test]
fn host_less_generation_displays_but_never_authorizes() {
    let store = AdmissionStore::new();
    let published = store
        .publish(
            candidate("branch_prefix = \"a/\"\n").mark_hosts_unavailable(),
            0,
        )
        .unwrap();
    assert_eq!(store.display().unwrap().revision(), published.revision());
    assert_eq!(
        store.authorize(&published.revision()).unwrap_err().reason,
        StaleConfigReason::HostsUnavailable
    );
    // A later healthy publication restores authority.
    let healthy = store
        .publish(candidate("branch_prefix = \"a/\"\n"), 1)
        .unwrap();
    assert!(store.authorize(&healthy.revision()).is_ok());
}
