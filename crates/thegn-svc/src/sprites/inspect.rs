//! One sprite's status over the org API: `GET /v1/sprites/{name}`. A read
//! that never wakes the sprite (no exec, no resume), used by `thegn env adopt`
//! and the opt-in lifetime reaper.
//!
//! b162 spine stub: lane 2 fills in the request and the parser.

use anyhow::bail;

/// What `GET /v1/sprites/{name}` reports about one sprite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpriteInfo {
    pub name: String,
    pub status: String,
    pub created_at: Option<String>,
}

/// Parse one sprite object from the Sprites API.
pub fn parse_info(_v: &serde_json::Value) -> anyhow::Result<SpriteInfo> {
    bail!("stub")
}

impl crate::provider::SpritesProvider {
    /// GET /v1/sprites/{name}; 404 → Ok(None). Never wakes the sprite (org API).
    pub async fn inspect(&self, _name: &str) -> anyhow::Result<Option<SpriteInfo>> {
        Ok(None)
    }
}
