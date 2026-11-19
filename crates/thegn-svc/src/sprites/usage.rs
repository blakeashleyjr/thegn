//! Sprite usage for `thegn fleet cost`: CPU seconds and reserved GB-hours from
//! `GET /v1/usage/{name}?start=<created_at>`. Runs only from the explicit verb.
//!
//! Spine stub (b160 lane 1): the signatures are final; lane 5 replaces the
//! bodies.

use anyhow::Result;

/// One sprite's usage since a start time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpriteUsage {
    pub cpu_seconds: f64,
    pub reserved_gb_hours: f64,
    /// The API reported an incomplete window.
    pub partial: bool,
}

/// Parse a usage response.
///
/// Spine stub: reads nothing and reports a partial, empty window.
pub fn parse_usage(_v: &serde_json::Value) -> Result<SpriteUsage> {
    Ok(SpriteUsage {
        partial: true,
        ..SpriteUsage::default()
    })
}

impl crate::provider::SpritesProvider {
    /// Usage for sprite `name` since `start_rfc3339` (`None` = unavailable).
    ///
    /// Spine stub: always unavailable.
    pub async fn usage_since(
        &self,
        name: &str,
        start_rfc3339: &str,
    ) -> Result<Option<SpriteUsage>> {
        tracing::debug!(
            target: "thegn::sprites",
            sprite = name,
            start = start_rfc3339,
            "sprites: usage not read yet"
        );
        Ok(None)
    }
}
