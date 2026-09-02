//! Stable transport/storage shape for the sidebar git glyph cache.
//!
//! The cache used to persist a positional tuple. Keep accepting that shape so
//! upgrades do not need a migration, but write a named record from now on.

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GlyphRow {
    pub(crate) dirty: bool,
    pub(crate) ahead: usize,
    pub(crate) behind: usize,
    pub(crate) branch: Option<String>,
    pub(crate) repo_root: String,
    pub(crate) add: u32,
    pub(crate) del: u32,
    pub(crate) branch_diff: Option<(u32, u32)>,
    #[serde(default)]
    pub(crate) submodule_dirty: bool,
}

#[derive(Deserialize)]
struct NamedGlyphRow {
    dirty: bool,
    ahead: usize,
    behind: usize,
    branch: Option<String>,
    repo_root: String,
    add: u32,
    del: u32,
    branch_diff: Option<(u32, u32)>,
    #[serde(default)]
    submodule_dirty: bool,
}

impl From<NamedGlyphRow> for GlyphRow {
    fn from(row: NamedGlyphRow) -> Self {
        Self {
            dirty: row.dirty,
            ahead: row.ahead,
            behind: row.behind,
            branch: row.branch,
            repo_root: row.repo_root,
            add: row.add,
            del: row.del,
            branch_diff: row.branch_diff,
            submodule_dirty: row.submodule_dirty,
        }
    }
}

impl<'de> Deserialize<'de> for GlyphRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Object(_) => serde_json::from_value::<NamedGlyphRow>(value)
                .map(Into::into)
                .map_err(serde::de::Error::custom),
            serde_json::Value::Array(values) => {
                if values.len() < 8 {
                    return Err(serde::de::Error::custom(
                        "legacy glyph row needs eight elements",
                    ));
                }
                let get = |index: usize| values[index].clone();
                Ok(Self {
                    dirty: serde_json::from_value(get(0)).map_err(serde::de::Error::custom)?,
                    ahead: serde_json::from_value(get(1)).map_err(serde::de::Error::custom)?,
                    behind: serde_json::from_value(get(2)).map_err(serde::de::Error::custom)?,
                    branch: serde_json::from_value(get(3)).map_err(serde::de::Error::custom)?,
                    repo_root: serde_json::from_value(get(4)).map_err(serde::de::Error::custom)?,
                    add: serde_json::from_value(get(5)).map_err(serde::de::Error::custom)?,
                    del: serde_json::from_value(get(6)).map_err(serde::de::Error::custom)?,
                    branch_diff: serde_json::from_value(get(7))
                        .map_err(serde::de::Error::custom)?,
                    // Legacy arrays predate this field and therefore mean
                    // "not observed", not a fabricated dirty result.
                    submodule_dirty: false,
                })
            }
            _ => Err(serde::de::Error::custom(
                "glyph row must be a named record or legacy array",
            )),
        }
    }
}
