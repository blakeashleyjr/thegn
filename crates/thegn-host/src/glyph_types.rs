//! Stable transport/storage shape for the sidebar git glyph cache.
//!
//! The cache used to persist a positional tuple. Keep accepting that shape so
//! upgrades do not need a migration, but write a named record from now on.

use serde::{Deserialize, Deserializer, Serialize};
use thegn_svc::git::DiffCount;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GlyphRow {
    pub(crate) dirty: bool,
    pub(crate) ahead: usize,
    pub(crate) behind: usize,
    pub(crate) branch: Option<String>,
    pub(crate) repo_root: String,
    #[serde(with = "count_serde")]
    pub(crate) add: DiffCount,
    #[serde(with = "count_serde")]
    pub(crate) del: DiffCount,
    #[serde(with = "diff_pair_serde")]
    pub(crate) branch_diff: Option<(DiffCount, DiffCount)>,
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
    #[serde(with = "count_serde")]
    add: DiffCount,
    #[serde(with = "count_serde")]
    del: DiffCount,
    #[serde(with = "diff_pair_serde")]
    branch_diff: Option<(DiffCount, DiffCount)>,
    #[serde(default)]
    submodule_dirty: bool,
}

mod count_serde {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    use thegn_svc::git::DiffCount;

    pub(super) fn serialize<S>(count: &DiffCount, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match count {
            DiffCount::Exact(n) => serializer.serialize_u32(*n),
            DiffCount::Binary => serializer.serialize_str("binary"),
            DiffCount::Capped => serializer.serialize_str("capped"),
            DiffCount::Unavailable => serializer.serialize_str("unavailable"),
        }
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<DiffCount, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        from_value(value).map_err(D::Error::custom)
    }

    pub(super) fn from_value(value: serde_json::Value) -> Result<DiffCount, &'static str> {
        match value {
            serde_json::Value::Number(number) => number
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(DiffCount::Exact)
                .ok_or("diff count must be a u32"),
            serde_json::Value::String(value) => match value.as_str() {
                "binary" => Ok(DiffCount::Binary),
                "capped" => Ok(DiffCount::Capped),
                "unavailable" => Ok(DiffCount::Unavailable),
                _ => Err("unknown diff count"),
            },
            _ => Err("diff count must be a number or known string"),
        }
    }
}

mod diff_pair_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error, ser::SerializeSeq};
    use thegn_svc::git::DiffCount;

    pub(super) fn serialize<S>(
        pair: &Option<(DiffCount, DiffCount)>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match pair {
            None => serializer.serialize_none(),
            Some((added, deleted)) => {
                let mut seq = serializer.serialize_seq(Some(2))?;
                seq.serialize_element(&CountRef(*added))?;
                seq.serialize_element(&CountRef(*deleted))?;
                seq.end()
            }
        }
    }

    struct CountRef(DiffCount);
    impl Serialize for CountRef {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            super::count_serde::serialize(&self.0, serializer)
        }
    }

    pub(super) fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Option<(DiffCount, DiffCount)>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<Vec<serde_json::Value>>::deserialize(deserializer)?;
        match value {
            None => Ok(None),
            Some(values) if values.len() == 2 => {
                let added =
                    super::count_serde::from_value(values[0].clone()).map_err(D::Error::custom)?;
                let deleted =
                    super::count_serde::from_value(values[1].clone()).map_err(D::Error::custom)?;
                Ok(Some((added, deleted)))
            }
            Some(_) => Err(D::Error::custom("branch diff must contain two counts")),
        }
    }
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
                    add: count_serde::from_value(get(5)).map_err(serde::de::Error::custom)?,
                    del: count_serde::from_value(get(6)).map_err(serde::de::Error::custom)?,
                    branch_diff: {
                        let pair = serde_json::from_value::<Option<Vec<serde_json::Value>>>(get(7))
                            .map_err(serde::de::Error::custom)?;
                        match pair {
                            None => None,
                            Some(values) if values.len() == 2 => Some((
                                count_serde::from_value(values[0].clone())
                                    .map_err(serde::de::Error::custom)?,
                                count_serde::from_value(values[1].clone())
                                    .map_err(serde::de::Error::custom)?,
                            )),
                            Some(_) => {
                                return Err(serde::de::Error::custom(
                                    "branch diff must contain two counts",
                                ));
                            }
                        }
                    },
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

/// Sidebar wording for non-exact diff observations. Exact counts remain numeric
/// at the render sites; unavailable values are not persisted as fresh rows.
pub(crate) fn sidebar_count_label(count: DiffCount) -> Option<&'static str> {
    match count {
        DiffCount::Binary => Some("bin"),
        DiffCount::Capped => Some("capped"),
        DiffCount::Exact(_) | DiffCount::Unavailable => None,
    }
}
