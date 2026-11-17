//! The automation kill switch: `automations.pause` / `automations.resume`
//! (THE-2197).
//!
//! Both verbs flip the durable `ui_state` flag in
//! [`thegn_core::automation_control`]; the runtime reads it per event before
//! admission, so a paused event is audited as `paused` and nothing executes.
//! Neither takes an argument (the MCP spec has none, so the request struct
//! denies unknown fields). Pause is Write and a Lead may call it; resume is
//! Admin, never MCP, and refused for a Lead-confined caller by
//! `thegn_core::lead_confine`: a Lead can stop automations, never restart them.

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;

/// The (empty) request body of both verbs.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PauseRequest {}

/// The reply of both verbs: the flag as it now stands.
#[derive(Debug, serde::Serialize)]
struct PauseReply {
    paused: bool,
}

/// Set the pause flag to `paused` and reply with it.
async fn set(svc: &DaemonService, req: serde_json::Value, paused: bool) -> JsonResult {
    let PauseRequest {} = decode(req)?;
    svc.with_db(move |db| thegn_core::automation_control::set_paused(db, paused))
        .await?;
    tracing::info!(
        target: "thegn::automation",
        paused,
        "automation pause flag set"
    );
    encode(&PauseReply { paused })
}

/// Answer `automations.pause`.
pub(crate) async fn pause(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    set(svc, req, true).await
}

/// Answer `automations.resume` (Admin; the transport refuses a Lead).
pub(crate) async fn resume(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    set(svc, req, false).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::IdleTransition;
    use crate::daemon::tombstone::{MAX_TOMBSTONES, TOMBSTONE_TTL_MS};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use thegn_core::{db::Db, graveyard::Graveyard};
    use thegn_svc::control::ControlError;
    use tokio::sync::{broadcast, mpsc};

    fn service() -> (DaemonService, tempfile::TempDir) {
        let profile = tempfile::Builder::new()
            .prefix("lead-automations-pause-")
            .tempdir()
            .expect("private profile tempdir");
        let (events, _rx) = broadcast::channel(16);
        let (idle_tx, _idle_rx) = mpsc::unbounded_channel::<IdleTransition>();
        let svc = DaemonService {
            daemon_id: "test".into(),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            actor_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            accepting: AtomicBool::new(true),
            tombs: Arc::new(tokio::sync::Mutex::new(Graveyard::new(
                MAX_TOMBSTONES,
                TOMBSTONE_TTL_MS,
            ))),
            native_ids: Mutex::new(HashMap::new()),
            spawning_relaunches: Arc::new(Mutex::new(HashSet::new())),
            launches: Mutex::new(crate::daemon::stage_launch::LaunchRegistry::default()),
            events,
            db: Arc::new(Mutex::new(Db::open_memory().expect("db"))),
            grace_ms: 0,
            idle_tx,
            shutdown: tokio_util::sync::CancellationToken::new(),
            config: Arc::new(thegn_core::config::Config::default()),
            profile_root: profile.path().to_path_buf(),
            endpoint: "/tmp/test.sock".into(),
        };
        (svc, profile)
    }

    async fn flag(svc: &DaemonService) -> bool {
        svc.with_db(|db| Ok(thegn_core::automation_control::paused(db)))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn pause_and_resume_round_trip_the_durable_flag() {
        let (svc, _profile) = service();
        assert!(!flag(&svc).await);
        assert_eq!(
            pause(&svc, json!({})).await.unwrap(),
            json!({"paused": true})
        );
        assert!(flag(&svc).await);
        // Idempotent.
        assert_eq!(
            pause(&svc, json!({})).await.unwrap(),
            json!({"paused": true})
        );
        assert!(flag(&svc).await);
        assert_eq!(
            resume(&svc, json!({})).await.unwrap(),
            json!({"paused": false})
        );
        assert!(!flag(&svc).await);
        assert_eq!(
            resume(&svc, json!({})).await.unwrap(),
            json!({"paused": false})
        );
        assert!(!flag(&svc).await);
    }

    #[tokio::test]
    async fn unknown_fields_are_refused_and_leave_the_flag_alone() {
        let (svc, _profile) = service();
        assert!(matches!(
            pause(&svc, json!({"rule": "x"})).await,
            Err(ControlError::InvalidArgument(_))
        ));
        assert!(!flag(&svc).await);
        pause(&svc, json!({})).await.unwrap();
        assert!(matches!(
            resume(&svc, json!({"force": true})).await,
            Err(ControlError::InvalidArgument(_))
        ));
        assert!(flag(&svc).await, "a refused resume must not clear the flag");
    }
}
