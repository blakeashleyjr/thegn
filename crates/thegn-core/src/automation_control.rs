//! The automation kill switch (THE-2192): a durable pause flag.
//!
//! Stored as the `ui_state` row (`automation`, `paused`), following the
//! executor's `automation_merge_origin` precedent, so it needs no schema and
//! survives restarts. The runtime reads it per event before admission; a
//! paused event is audited as `SkipReason::Paused` and nothing executes.
//! `automations.pause` sets it (a Lead may); `automations.resume` (Admin,
//! never MCP) clears it.

use anyhow::Result;

use crate::store::WorkspaceStore;

/// `ui_state` scope of the automation control rows.
pub const SCOPE: &str = "automation";
/// `ui_state` key of the pause flag; present (value `"1"`) means paused.
pub const PAUSED_KEY: &str = "paused";

/// Whether the runtime is paused. A read error counts as NOT paused (with a
/// warning): the flag is a brake the operator set deliberately, and a broken
/// read must not silently turn every rule off.
pub fn paused(store: &dyn WorkspaceStore) -> bool {
    match store.get_ui_state(SCOPE, PAUSED_KEY) {
        Ok(value) => value.is_some_and(|value| value == "1"),
        Err(error) => {
            tracing::warn!(
                target: "thegn::automation",
                %error,
                "could not read the automation pause flag; treating as not paused"
            );
            false
        }
    }
}

/// The pause flag with read errors surfaced; b155 callers treat an error as paused.
pub fn paused_strict(store: &dyn WorkspaceStore) -> Result<bool> {
    Ok(store
        .get_ui_state(SCOPE, PAUSED_KEY)?
        .is_some_and(|v| v == "1"))
}

/// `automations.halt` (Admin; HTTP + CLI; b155 6f): kill every live
/// automation-started or Lead-opened session unless `keep_sessions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaltRequest {
    #[serde(default)]
    pub keep_sessions: bool,
}

/// Set or clear the pause flag.
pub fn set_paused(store: &dyn WorkspaceStore, paused: bool) -> Result<()> {
    if paused {
        store.set_ui_state(SCOPE, PAUSED_KEY, "1")
    } else {
        store.del_ui_state(SCOPE, PAUSED_KEY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halt_body_is_strict_and_defaults_to_killing() {
        let empty: HaltRequest = serde_json::from_str("{}").unwrap();
        assert!(!empty.keep_sessions);
        let keep: HaltRequest = serde_json::from_str(r#"{"keep_sessions":true}"#).unwrap();
        assert!(keep.keep_sessions);
        assert!(serde_json::from_str::<HaltRequest>(r#"{"all":true}"#).is_err());
    }

    #[test]
    fn pause_flag_round_trips_and_defaults_off() {
        let db = crate::db::Db::open_memory().unwrap();
        assert!(!paused(&db));
        set_paused(&db, true).unwrap();
        assert!(paused(&db));
        set_paused(&db, true).unwrap();
        assert!(paused(&db));
        set_paused(&db, false).unwrap();
        assert!(!paused(&db));
        set_paused(&db, false).unwrap();
        assert!(!paused(&db));
    }

    #[test]
    fn strict_pause_flag_defaults_off_and_reads_set() {
        let db = crate::db::Db::open_memory().unwrap();
        assert!(!paused_strict(&db).unwrap());
        set_paused(&db, true).unwrap();
        assert!(paused_strict(&db).unwrap());
    }

    #[test]
    fn only_the_canonical_value_pauses() {
        let db = crate::db::Db::open_memory().unwrap();
        db.set_ui_state(SCOPE, PAUSED_KEY, "0").unwrap();
        assert!(!paused(&db));
    }
}
