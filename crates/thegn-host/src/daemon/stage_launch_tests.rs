use std::sync::Mutex;

use anyhow::Context;
use thegn_svc::control::client::{ControlBoundError, ControlRequestError};
use thegn_svc::control::{OpenSpec, SessionInfo};

use super::{
    LaunchAdmit, LaunchGuard, LaunchRegistry, STAGE_LAUNCH_ENV, launch_id_of,
    open_error_is_ambiguous, open_with_reconcile,
};

fn info(id: &str) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        ..Default::default()
    }
}

#[test]
fn stage_launch_id_is_32_lower_hex_and_unique() {
    let first = super::mint_launch_id().expect("entropy available");
    let second = super::mint_launch_id().expect("entropy available");
    assert_eq!(first.len(), 32);
    assert!(
        first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_ne!(first, second);
}

#[test]
fn stage_launch_id_rejects_malformed_values() {
    let values = [
        String::new(),
        "0".repeat(31),
        "0".repeat(33),
        "A".repeat(32),
        "g".repeat(32),
    ];
    for value in values {
        let spec = OpenSpec {
            env: vec![(STAGE_LAUNCH_ENV.into(), value.clone())],
            ..Default::default()
        };
        assert!(matches!(launch_id_of(&spec), Some(Err(_))), "{value:?}");
    }
}

#[test]
fn stage_launch_registry_replays_opened_and_refused_entries() {
    let mut registry = LaunchRegistry::default();
    assert!(matches!(
        registry.begin("a", Some(1), Some("/w"), 10),
        LaunchAdmit::Fresh
    ));
    registry.commit("a", &info("session-a"));
    assert!(
        matches!(registry.begin("a", Some(1), Some("/w"), 11), LaunchAdmit::Replay(ref replay) if replay.id == "session-a")
    );

    assert!(matches!(
        registry.begin("b", Some(2), None, 10),
        LaunchAdmit::Fresh
    ));
    registry.refuse("b");
    assert!(
        matches!(registry.begin("b", Some(2), None, 11), LaunchAdmit::Refused(message) if message.contains("first open"))
    );
}

#[test]
fn stage_launch_registry_refuses_reuse_for_another_row_or_worktree() {
    let mut registry = LaunchRegistry::default();
    assert!(matches!(
        registry.begin("a", Some(1), Some("/w"), 10),
        LaunchAdmit::Fresh
    ));
    assert!(matches!(
        registry.begin("a", Some(2), Some("/w"), 11),
        LaunchAdmit::Mismatch
    ));
    assert!(matches!(
        registry.begin("a", Some(1), Some("/other"), 11),
        LaunchAdmit::Mismatch
    ));
}

#[test]
fn stage_launch_registry_reports_still_opening() {
    let mut registry = LaunchRegistry::default();
    assert!(matches!(
        registry.begin("a", Some(1), None, 10),
        LaunchAdmit::Fresh
    ));
    assert!(matches!(
        registry.begin("a", Some(1), None, 11),
        LaunchAdmit::StillOpening
    ));
}

#[test]
fn stage_launch_registry_is_bounded_and_never_evicts_opening() {
    let mut registry = LaunchRegistry::default();
    for index in 0..super::MAX_TOMBSTONES {
        assert!(matches!(
            registry.begin(&format!("{index}"), None, None, index as i64),
            LaunchAdmit::Fresh
        ));
    }
    assert!(matches!(
        registry.begin("overflow", None, None, 100),
        LaunchAdmit::Refused(_)
    ));

    registry.refuse("0");
    assert!(matches!(
        registry.begin("replacement", None, None, 101),
        LaunchAdmit::Fresh
    ));
    assert!(!registry.entries.contains_key("0"));
    assert!(registry.entries.contains_key("1"));
    assert_eq!(registry.entries.len(), super::MAX_TOMBSTONES);
}

#[test]
fn stage_launch_dropped_guard_records_a_refusal() {
    let registry = Mutex::new(LaunchRegistry::default());
    assert!(matches!(
        registry.lock().unwrap().begin("a", None, None, 10),
        LaunchAdmit::Fresh
    ));
    drop(LaunchGuard::new(&registry, "a"));
    assert!(matches!(
        registry.lock().unwrap().begin("a", None, None, 11),
        LaunchAdmit::Refused(_)
    ));
}

#[test]
fn stage_launch_open_error_classification() {
    let status = anyhow::Error::new(ControlRequestError::new(409, "conflict"));
    assert!(!open_error_is_ambiguous(&status));
    assert!(!open_error_is_ambiguous(&status.context("wrapped")));
    assert!(open_error_is_ambiguous(&anyhow::Error::new(
        ControlBoundError::RequestTimeout
    )));
    assert!(open_error_is_ambiguous(&anyhow::Error::new(
        std::io::Error::other("reset")
    )));
}

#[tokio::test]
async fn stage_launch_reconcile_retries_once_after_an_ambiguous_error() {
    let calls = std::cell::Cell::new(0);
    let opened = info("same-session");
    let result = open_with_reconcile(|| {
        let call = calls.get();
        calls.set(call + 1);
        std::future::ready(if call == 0 {
            Err(anyhow::Error::new(ControlBoundError::RequestTimeout))
        } else {
            Ok(opened.clone())
        })
    })
    .await
    .expect("retry adopts session");
    assert_eq!(result.id, "same-session");
    assert_eq!(calls.get(), 2);
}

#[tokio::test]
async fn stage_launch_reconcile_never_retries_a_typed_refusal() {
    let calls = std::cell::Cell::new(0);
    let result = open_with_reconcile(|| {
        calls.set(calls.get() + 1);
        std::future::ready(Err(anyhow::Error::new(ControlRequestError::new(
            409, "refused",
        ))))
    })
    .await;
    assert!(matches!(result, Err(error) if error.downcast_ref::<ControlRequestError>().is_some()));
    assert_eq!(calls.get(), 1);
}
