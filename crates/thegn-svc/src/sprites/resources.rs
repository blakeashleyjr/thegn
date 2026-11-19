//! The sprite memory policy: `[env.<n>.provider] min_ram_gb > 0` sets
//! `limit_mb = min_ram_gb × 1024` (autoscale unset) after create and again,
//! with a GET readback, at each provision. Sprites have a fixed 8 vCPU, so
//! there is no CPU knob.
//!
//! Spine stub (b160 lane 1): the signatures are final; lane 4 replaces the
//! request bodies.

use anyhow::Result;

use super::SpritesHttp;

/// Apply the memory policy, then read it back; a mismatch is an error.
///
/// Spine stub: applies nothing.
pub(crate) async fn apply(http: &SpritesHttp<'_>, limit_mb: u32) -> Result<()> {
    tracing::debug!(
        target: "thegn::sprites",
        sprite = http.name,
        limit_mb,
        "sprites: memory policy not applied yet"
    );
    Ok(())
}

/// The sprite's current memory limit in MiB (`None` = no limit set).
///
/// Spine stub: reports no limit.
#[allow(dead_code)] // Lane 4 wires the provision-time readback.
pub(crate) async fn read(http: &SpritesHttp<'_>) -> Result<Option<u32>> {
    tracing::debug!(target: "thegn::sprites", sprite = http.name, "sprites: memory policy not read yet");
    Ok(None)
}

/// The policy request body: `{"memory":{"limit_mb":N}}`.
pub fn policy_body(limit_mb: u32) -> serde_json::Value {
    serde_json::json!({ "memory": { "limit_mb": limit_mb } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_body_sets_only_the_memory_limit() {
        assert_eq!(
            policy_body(2048),
            serde_json::json!({ "memory": { "limit_mb": 2048 } })
        );
    }
}
