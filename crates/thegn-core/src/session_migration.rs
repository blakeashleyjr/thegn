//! Pure policy and row carriers for moving a persisted session presentation
//! between profile databases.
//!
//! This module deliberately knows nothing about terminals, daemons, config, or
//! credentials. JSON and command fields remain opaque; the target compositor is
//! responsible for interpreting them after resurrection.

use crate::issue::{AgentDispatch, DispatchNote};
use crate::models::{GroupTabRow, TabGroupRow, WorktreeRow};
use serde::de::DeserializeSeed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A worktree row explicitly allowlisted for migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationWorktree {
    pub worktree: String,
    pub session_name: String,
    pub tab_name: String,
    pub repo_root: String,
    pub branch: String,
    pub agent: String,
    pub created_at: i64,
    pub location: String,
    pub position: i64,
    pub sandbox_backend: Option<String>,
    pub observed_backend: Option<String>,
    pub folder_id: Option<i64>,
    pub env_name: Option<String>,
}

impl From<WorktreeRow> for MigrationWorktree {
    fn from(row: WorktreeRow) -> Self {
        Self {
            worktree: row.worktree,
            session_name: row.session_name,
            tab_name: row.tab_name,
            repo_root: row.repo_root,
            branch: row.branch,
            agent: row.agent,
            created_at: row.created_at,
            location: row.location,
            position: row.position,
            sandbox_backend: row.sandbox_backend,
            observed_backend: row.observed_backend,
            folder_id: row.folder_id,
            env_name: row.env_name,
        }
    }
}

/// A selected worktree group and all of its persisted tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationGroup {
    pub session_name: String,
    pub name: String,
    pub kind: String,
    pub worktree: String,
    pub ordinal: i64,
    pub active_tab: i64,
    pub tabs: Vec<MigrationTab>,
}

/// An explicitly allowlisted tab payload. Pane cwd hints outside the moved
/// worktree are dropped at import. `pane_sessions` is retained in the source
/// snapshot for liveness preflight, then cleared at import because daemon
/// session ids are profile-local.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationTab {
    pub session_name: String,
    pub group_name: String,
    pub ordinal: i64,
    pub title: String,
    pub pane_tree: String,
    pub focused_pane: i64,
    pub pane_cwds: String,
    pub pane_cmds: String,
    pub pane_sessions: String,
    pub scrollback_snapshot: String,
}

/// A sidebar key/value row. Only `scope = "sidebar"` and selected, anchored
/// key segments are admitted to a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationUiState {
    pub scope: String,
    pub key: String,
    pub value: Option<String>,
}

/// A dispatch row with its source id retained solely for deterministic remap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationDispatch {
    pub source_id: i64,
    pub issue_id: String,
    pub worktree_path: String,
    pub agent_name: String,
    pub dispatched_at_ms: i64,
    /// Preserve the exact stored value. The ordinary roster reader maps
    /// unfamiliar legacy/future values to `Unknown`, but migration must not
    /// write that lossy coercion back into the target database.
    pub status: String,
    pub stage: Option<String>,
    pub parent_id: Option<i64>,
    pub session_id: Option<String>,
    pub artifact_path: Option<String>,
    pub note: Option<String>,
    pub chunk_path: Option<String>,
    pub report: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_scope: Option<String>,
}

impl From<AgentDispatch> for MigrationDispatch {
    fn from(row: AgentDispatch) -> Self {
        Self {
            source_id: row.id,
            issue_id: row.issue_id,
            worktree_path: row.worktree_path,
            agent_name: row.agent_name,
            dispatched_at_ms: row.dispatched_at_ms,
            status: row.status.as_str().to_string(),
            stage: row.stage,
            parent_id: row.parent_id,
            session_id: row.session_id,
            artifact_path: row.artifact_path,
            note: row.note,
            chunk_path: row.chunk_path,
            report: row.report,
            chunk_scope: None,
        }
    }
}

/// A dispatch note with its source id retained only while importing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationNote {
    pub source_id: i64,
    pub dispatch_id: i64,
    pub created_at_ms: i64,
    pub text: String,
}

impl From<DispatchNote> for MigrationNote {
    fn from(row: DispatchNote) -> Self {
        Self {
            source_id: row.id,
            dispatch_id: row.dispatch_id,
            created_at_ms: row.created_at_ms,
            text: row.text,
        }
    }
}

/// The source snapshot. Every field in this type is either a selected cache row
/// or explicitly selected session state; import policy decides whether content
/// history is copied. Credentials and global caches have no carrier here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBundle {
    pub source_profile: String,
    pub target_profile: String,
    pub session_name: String,
    pub worktree_path: String,
    pub worktree: Option<MigrationWorktree>,
    pub groups: Vec<MigrationGroup>,
    pub ui_state: Vec<MigrationUiState>,
    pub dispatches: Vec<MigrationDispatch>,
    pub notes: Vec<MigrationNote>,
    pub pin_state: Option<String>,
    pub pin_updated_at: Option<i64>,
}

/// Total serialized size a migration bundle may have (primary decision, batch 70).
pub const MAX_MIGRATION_BUNDLE_BYTES: usize = 4 * 1024 * 1024;
/// Deepest JSON nesting a migrated tab field may have (primary decision, batch 70).
pub const MAX_MIGRATION_JSON_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationBoundsError {
    TooLarge {
        bytes: usize,
        max: usize,
    },
    TooDeep {
        group: String,
        ordinal: i64,
        field: &'static str,
        max: usize,
    },
    DuplicateKey {
        group: String,
        ordinal: i64,
        field: &'static str,
        key: String,
    },
}

impl std::fmt::Display for MigrationBoundsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes, max } => {
                write!(f, "session bundle is {bytes} bytes; the limit is {max}")
            }
            Self::TooDeep {
                group,
                ordinal,
                field,
                max,
            } => write!(
                f,
                "group {group:?} tab {ordinal} {field} nests deeper than {max}"
            ),
            Self::DuplicateKey {
                group,
                ordinal,
                field,
                key,
            } => write!(
                f,
                "group {group:?} tab {ordinal} {field} repeats the key {key:?}"
            ),
        }
    }
}

impl std::error::Error for MigrationBoundsError {}

/// Refuse migration bundles whose opaque tab payloads exceed safe parse bounds.
pub fn check_bundle_bounds(bundle: &MigrationBundle) -> Result<(), MigrationBoundsError> {
    let bytes = match serde_json::to_vec(bundle) {
        Ok(encoded) => encoded.len(),
        Err(_) => usize::MAX,
    };
    if bytes > MAX_MIGRATION_BUNDLE_BYTES {
        return Err(MigrationBoundsError::TooLarge {
            bytes,
            max: MAX_MIGRATION_BUNDLE_BYTES,
        });
    }

    for group in &bundle.groups {
        for tab in &group.tabs {
            for (field, value) in [
                ("pane_tree", tab.pane_tree.as_str()),
                ("pane_cwds", tab.pane_cwds.as_str()),
                ("pane_cmds", tab.pane_cmds.as_str()),
                ("pane_sessions", tab.pane_sessions.as_str()),
                ("scrollback_snapshot", tab.scrollback_snapshot.as_str()),
            ] {
                if value.trim().is_empty() {
                    continue;
                }
                if json_nesting_depth(value) > MAX_MIGRATION_JSON_DEPTH {
                    return Err(MigrationBoundsError::TooDeep {
                        group: group.name.clone(),
                        ordinal: tab.ordinal,
                        field,
                        max: MAX_MIGRATION_JSON_DEPTH,
                    });
                }
                if let Err(error) = parse_no_duplicate_keys(value)
                    && let Some(key) = duplicate_key_from_error(&error)
                {
                    return Err(MigrationBoundsError::DuplicateKey {
                        group: group.name.clone(),
                        ordinal: tab.ordinal,
                        field,
                        key: truncate_chars(&key, 64),
                    });
                }
            }
        }
    }
    Ok(())
}

const DUP_MARKER_PREFIX: &str = "duplicate-json-key-hex:";

fn duplicate_key_marker(key: &str) -> String {
    let mut marker = String::with_capacity(DUP_MARKER_PREFIX.len() + key.len() * 2 + 1);
    marker.push_str(DUP_MARKER_PREFIX);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in key.bytes() {
        marker.push(char::from(HEX[usize::from(byte >> 4)]));
        marker.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    marker.push(':');
    marker
}

fn duplicate_key_from_error(error: &str) -> Option<String> {
    let encoded = error.strip_prefix(DUP_MARKER_PREFIX)?.split(':').next()?;
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let bytes = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn json_nesting_depth(value: &str) -> usize {
    let mut depth: usize = 0;
    let mut maximum = 0;
    let mut in_string = false;
    let mut escaped = false;
    for byte in value.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'[' | b'{' => {
                    depth += 1;
                    maximum = maximum.max(depth);
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    maximum
}

fn parse_no_duplicate_keys(value: &str) -> Result<(), String> {
    let mut deserializer = serde_json::Deserializer::from_str(value);
    NoDuplicateKeys
        .deserialize(&mut deserializer)
        .and_then(|()| deserializer.end())
        .map_err(|error| error.to_string())
}

struct NoDuplicateKeys;

impl<'de> serde::de::DeserializeSeed<'de> for NoDuplicateKeys {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_any(NoDuplicateKeysVisitor)
    }
}

struct NoDuplicateKeysVisitor;

impl<'de> serde::de::Visitor<'de> for NoDuplicateKeysVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("any JSON value without duplicate object keys")
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        while sequence.next_element_seed(NoDuplicateKeys)?.is_some() {}
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(duplicate_key_marker(&key)));
            }
            map.next_value_seed(NoDuplicateKeys)?;
        }
        Ok(())
    }
}

/// The target's allowlisted rows. `groups`/`tabs` may contain other groups so
/// group-key collisions can be checked without querying an unallowlisted table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MigrationTarget {
    pub worktree: Option<MigrationWorktree>,
    pub groups: Vec<MigrationGroup>,
    pub ui_state: Vec<MigrationUiState>,
    pub dispatches: Vec<MigrationDispatch>,
    pub notes: Vec<MigrationNote>,
    pub pin_state: Option<String>,
    pub pin_updated_at: Option<i64>,
    pub active_tab: Option<String>,
    pub updated_at: Option<i64>,
}

/// Content history includes tab scrollback, pane relaunch commands, dispatch
/// reports, free-form dispatch notes, and dispatch note rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryPolicy {
    #[default]
    Drop,
    Include,
}

/// A pure, validated import decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    pub bundle: MigrationBundle,
    pub target: MigrationTarget,
    pub fingerprint: String,
    pub resumed: bool,
    pub history: HistoryPolicy,
}

/// A strict target conflict. Worktree registration is intentionally absent:
/// target metadata always wins for that row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationConflict {
    Group(String),
    UiState(String),
    PinState,
    PriorImport,
}

impl std::fmt::Display for MigrationConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Group(key) => write!(f, "target group conflicts: {key}"),
            Self::UiState(key) => write!(f, "target sidebar state conflicts: {key}"),
            Self::PinState => f.write_str("target running-pin state conflicts"),
            Self::PriorImport => f.write_str("target contains a different prior migration"),
        }
    }
}

impl std::error::Error for MigrationConflict {}

/// Per-table counts returned by a target import or source cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MigrationCounts {
    pub worktrees: usize,
    pub tab_groups: usize,
    pub group_tabs: usize,
    pub ui_state: usize,
    pub dispatches: usize,
    pub dispatch_notes: usize,
    pub attention: usize,
}

/// Result of a target transaction, including the fresh dispatch id map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationImportResult {
    pub counts: MigrationCounts,
    pub dispatch_id_map: BTreeMap<i64, i64>,
    pub fingerprint: String,
}

/// Exact source rows deleted after a target read-back is confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MigrationCleanupResult {
    pub counts: MigrationCounts,
    pub source_deleted: bool,
}

/// Host-supplied liveness facts used before a bundle is imported. Core keeps
/// this carrier free of daemon/client types; the host fills it from the source
/// control seam and uses the pane/dispatch ids retained in [`MigrationBundle`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MigrationLiveness {
    pub referenced_session_ids: Vec<String>,
    pub live_session_ids: Vec<String>,
    pub killed_session_ids: Vec<String>,
}

/// Select every group for an exact active-session/worktree pair and attach all
/// tabs belonging to those groups. Group names are never guessed from paths.
pub fn select_groups(
    active_session: &str,
    worktree_path: &str,
    groups: &[TabGroupRow],
    tabs: &[GroupTabRow],
) -> Vec<MigrationGroup> {
    let names: BTreeSet<String> = groups
        .iter()
        .filter(|g| g.worktree == worktree_path)
        .map(|g| g.name.clone())
        .collect();
    let mut selected = groups
        .iter()
        .filter(|g| g.worktree == worktree_path && names.contains(&g.name))
        .map(|g| MigrationGroup {
            session_name: active_session.to_string(),
            name: g.name.clone(),
            kind: g.kind.clone(),
            worktree: g.worktree.clone(),
            ordinal: g.ordinal,
            active_tab: g.active_tab,
            tabs: tabs
                .iter()
                .filter(|t| t.group_name == g.name)
                .map(|t| MigrationTab {
                    session_name: active_session.to_string(),
                    group_name: t.group_name.clone(),
                    ordinal: t.ordinal,
                    title: t.title.clone(),
                    pane_tree: t.pane_tree.clone(),
                    focused_pane: t.focused_pane,
                    pane_cwds: t.pane_cwds.clone(),
                    pane_cmds: t.pane_cmds.clone(),
                    pane_sessions: t.pane_sessions.clone(),
                    scrollback_snapshot: t.scrollback_snapshot.clone(),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    selected.sort_by_key(|g| (g.ordinal, g.name.clone()));
    for group in &mut selected {
        group.tabs.sort_by_key(|t| t.ordinal);
    }
    selected
}

/// Whether a sidebar key belongs to the exact `{kind}:{group}` segment. This
/// accepts child segments such as `pin:api/feature`, but never `pin:api-v2`.
pub fn sidebar_key_matches(key: &str, kind: &str, group: &str) -> bool {
    let Some(rest) = key.strip_prefix(&format!("{kind}:")) else {
        return false;
    };
    rest == group
        || rest
            .strip_prefix(group)
            .is_some_and(|tail| tail.starts_with('/'))
}

/// Select only sidebar collapse/pin families for the selected group names.
pub fn select_sidebar_state(
    rows: &[MigrationUiState],
    groups: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<MigrationUiState> {
    let groups: Vec<String> = groups.into_iter().map(|g| g.as_ref().to_string()).collect();
    let mut out = rows
        .iter()
        .filter(|row| {
            row.scope == "sidebar"
                && ["collapse", "pin", "pin_ordinal"].iter().any(|kind| {
                    groups
                        .iter()
                        .any(|g| sidebar_key_matches(&row.key, kind, g))
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// Build a source bundle from already-selected, allowlisted carriers.
#[allow(clippy::too_many_arguments)]
pub fn make_bundle(
    source_profile: impl Into<String>,
    target_profile: impl Into<String>,
    session_name: impl Into<String>,
    worktree_path: impl Into<String>,
    worktree: Option<MigrationWorktree>,
    mut groups: Vec<MigrationGroup>,
    mut ui_state: Vec<MigrationUiState>,
    mut dispatches: Vec<MigrationDispatch>,
    mut notes: Vec<MigrationNote>,
    pin_state: Option<String>,
    pin_updated_at: Option<i64>,
) -> MigrationBundle {
    groups.sort_by_key(|g| (g.ordinal, g.name.clone()));
    for group in &mut groups {
        group.tabs.sort_by_key(|t| t.ordinal);
    }
    ui_state.sort_by(|a, b| {
        (a.scope.as_str(), a.key.as_str()).cmp(&(b.scope.as_str(), b.key.as_str()))
    });
    dispatches.sort_by_key(|d| d.source_id);
    notes.sort_by_key(|n| (n.dispatch_id, n.created_at_ms, n.source_id));
    MigrationBundle {
        source_profile: source_profile.into(),
        target_profile: target_profile.into(),
        session_name: session_name.into(),
        worktree_path: worktree_path.into(),
        worktree,
        groups,
        ui_state,
        dispatches,
        notes,
        pin_state,
        pin_updated_at,
    }
}

/// Produce the pure target-first decision, including strict collision checks.
/// A target imported under the other history policy is refused; rerun with the
/// policy used for that import rather than silently merging the two forms.
pub fn plan_migration(
    bundle: MigrationBundle,
    target: MigrationTarget,
    history: HistoryPolicy,
) -> Result<MigrationPlan, MigrationConflict> {
    let imported = imported_bundle(&bundle, history);
    for source_group in &imported.groups {
        if target
            .groups
            .iter()
            .find(|g| g.session_name == source_group.session_name && g.name == source_group.name)
            .is_some_and(|target_group| {
                sanitized_group(target_group) != sanitized_group(source_group)
            })
        {
            return Err(MigrationConflict::Group(source_group.name.clone()));
        }
    }
    for source_key in &bundle.ui_state {
        if target
            .ui_state
            .iter()
            .find(|row| row.scope == source_key.scope && row.key == source_key.key)
            .is_some_and(|target_key| target_key.value != source_key.value)
        {
            return Err(MigrationConflict::UiState(source_key.key.clone()));
        }
    }
    if let (Some(source), Some(existing)) = (&bundle.pin_state, &target.pin_state)
        && source != existing
    {
        return Err(MigrationConflict::PinState);
    }

    let target_subset = target_subset(&bundle, &target);
    // A matching target is compared with the form this importer writes. A raw
    // pre-change copy of active rows or source-local paths is not an adopted
    // import and must be resolved by the operator before migration can proceed.
    let fingerprint = imported.fingerprint();
    let resumed = target_subset.fingerprint() == fingerprint && has_prior_rows(&target_subset);
    if !resumed
        && (!target_subset.dispatches.is_empty()
            || !target_subset.ui_state.is_empty()
            || target_subset.pin_state.is_some())
    {
        return Err(MigrationConflict::PriorImport);
    }
    Ok(MigrationPlan {
        bundle,
        target,
        fingerprint,
        resumed,
        history,
    })
}

fn has_prior_rows(bundle: &MigrationBundle) -> bool {
    !bundle.groups.is_empty()
        || !bundle.ui_state.is_empty()
        || !bundle.dispatches.is_empty()
        || bundle.pin_state.is_some()
}

/// Filter a target snapshot to the exact rows that a bundle would own.
pub fn target_subset(bundle: &MigrationBundle, target: &MigrationTarget) -> MigrationBundle {
    let names: BTreeSet<&str> = bundle.groups.iter().map(|g| g.name.as_str()).collect();
    let mut groups = target
        .groups
        .iter()
        .filter(|g| g.session_name == bundle.session_name && names.contains(g.name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    for group in &mut groups {
        group.tabs = target
            .groups
            .iter()
            .find(|candidate| {
                candidate.session_name == group.session_name && candidate.name == group.name
            })
            .map(|candidate| candidate.tabs.clone())
            .unwrap_or_default();
    }
    let source_keys: BTreeSet<(&str, &str)> = bundle
        .ui_state
        .iter()
        .map(|row| (row.scope.as_str(), row.key.as_str()))
        .collect();
    let ui_state = target
        .ui_state
        .iter()
        .filter(|row| source_keys.contains(&(row.scope.as_str(), row.key.as_str())))
        .cloned()
        .collect();
    make_bundle(
        bundle.source_profile.clone(),
        bundle.target_profile.clone(),
        bundle.session_name.clone(),
        bundle.worktree_path.clone(),
        target.worktree.clone(),
        groups,
        ui_state,
        target
            .dispatches
            .iter()
            .filter(|d| d.worktree_path == bundle.worktree_path)
            .cloned()
            .collect(),
        target.notes.clone(),
        bundle
            .pin_state
            .as_ref()
            .is_some()
            .then(|| target.pin_state.clone())
            .flatten(),
        target.pin_updated_at,
    )
}

fn sanitized_group(group: &MigrationGroup) -> MigrationGroup {
    let mut out = group.clone();
    for tab in &mut out.tabs {
        tab.pane_sessions.clear();
    }
    out
}

/// Allocate deterministic target ids in source-id order and rewrite only
/// in-set parents. Every daemon session id is cleared.
pub fn remap_dispatches(
    dispatches: &[MigrationDispatch],
    target_ids: &BTreeMap<i64, i64>,
) -> Vec<MigrationDispatch> {
    let in_set: BTreeSet<i64> = dispatches.iter().map(|d| d.source_id).collect();
    let mut out = dispatches.to_vec();
    out.sort_by_key(|d| d.source_id);
    for row in &mut out {
        row.source_id = target_ids.get(&row.source_id).copied().unwrap_or_default();
        row.parent_id = row
            .parent_id
            .filter(|parent| in_set.contains(parent))
            .and_then(|parent| target_ids.get(&parent).copied());
        row.session_id = None;
    }
    out
}

/// Convert a source dispatch into the safe form stored in the target profile.
/// Active workers are parked as abandoned; every daemon id and source-local
/// artifact/chunk locator is removed for all statuses.
pub fn imported_dispatch(row: &MigrationDispatch) -> MigrationDispatch {
    let mut imported = row.clone();
    imported.session_id = None;
    imported.artifact_path = None;
    imported.chunk_path = None;
    if crate::issue::AgentDispatchStatus::parse(&row.status).is_active() {
        imported.status = crate::issue::AgentDispatchStatus::Abandoned
            .as_str()
            .to_string();
        imported.note = Some(migrated_note(&row.status, row.note.as_deref()));
    }
    imported
}

fn migrated_note(status: &str, note: Option<&str>) -> String {
    let prefix = format!("[migrated while {}: not resumed]", status.trim());
    match note {
        Some(note) if !note.is_empty() => format!("{prefix} {note}"),
        _ => prefix,
    }
}

/// Return a copy of a migration bundle in the exact form imported by the
/// target, preserving row order and source ids for parent/note remapping. Pane
/// cwd hints outside the moved worktree are dropped in either history mode.
pub fn imported_bundle(bundle: &MigrationBundle, history: HistoryPolicy) -> MigrationBundle {
    let mut imported = bundle.clone();
    imported.dispatches = bundle.dispatches.iter().map(imported_dispatch).collect();
    for group in &mut imported.groups {
        for tab in &mut group.tabs {
            tab.pane_cwds = scoped_pane_cwds(&tab.pane_cwds, &bundle.worktree_path);
        }
    }
    if history == HistoryPolicy::Drop {
        for group in &mut imported.groups {
            for tab in &mut group.tabs {
                tab.scrollback_snapshot.clear();
                tab.pane_cmds.clear();
            }
        }
        for (source, target) in bundle.dispatches.iter().zip(&mut imported.dispatches) {
            target.report = None;
            target.note = if crate::issue::AgentDispatchStatus::parse(&source.status).is_active() {
                Some(migrated_note(&source.status, None))
            } else if source.status == crate::issue::AgentDispatchStatus::Abandoned.as_str()
                && source.note.as_deref().is_some_and(is_migration_prefix)
            {
                source.note.clone()
            } else {
                None
            };
        }
        imported.notes.clear();
    }
    imported
}

fn scoped_pane_cwds(pane_cwds: &str, worktree_path: &str) -> String {
    if pane_cwds.is_empty() {
        return String::new();
    }
    let Ok(cwds) = serde_json::from_str::<BTreeMap<u32, String>>(pane_cwds) else {
        return String::new();
    };
    let worktree = Path::new(worktree_path);
    let kept: BTreeMap<u32, String> = cwds
        .into_iter()
        .filter(|(_, cwd)| {
            !worktree_path.is_empty()
                && Path::new(cwd).is_absolute()
                && Path::new(cwd).starts_with(worktree)
        })
        .collect();
    serde_json::to_string(&kept).unwrap_or_default()
}

fn is_migration_prefix(note: &str) -> bool {
    let Some(status) = note
        .strip_prefix("[migrated while ")
        .and_then(|value| value.strip_suffix(": not resumed]"))
    else {
        return false;
    };
    crate::issue::AgentDispatchStatus::parse(status).is_active()
        && migrated_note(status, None) == note
}

/// A stable SHA-256 over sanitized rows. A migration plan computes this over
/// the imported dispatch form. Database ids, daemon ids, profile names, and
/// target-owned worktree metadata are intentionally not included.
pub fn fingerprint(bundle: &MigrationBundle) -> String {
    let material = FingerprintMaterial::from_bundle(bundle);
    let bytes = serde_json::to_vec(&material).expect("fingerprint carriers serialize");
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl MigrationBundle {
    pub fn fingerprint(&self) -> String {
        fingerprint(self)
    }
}

#[derive(Serialize)]
struct FingerprintMaterial {
    session_name: String,
    worktree_path: String,
    groups: Vec<MigrationGroupFingerprint>,
    ui_state: Vec<MigrationUiState>,
    dispatches: Vec<MigrationDispatchFingerprint>,
    notes: Vec<MigrationNoteFingerprint>,
    pin_state: Option<String>,
}

#[derive(Serialize)]
struct MigrationGroupFingerprint {
    session_name: String,
    name: String,
    kind: String,
    worktree: String,
    ordinal: i64,
    active_tab: i64,
    tabs: Vec<MigrationTab>,
}

#[derive(Serialize)]
struct MigrationDispatchFingerprint {
    issue_id: String,
    worktree_path: String,
    agent_name: String,
    dispatched_at_ms: i64,
    status: String,
    stage: Option<String>,
    parent_index: Option<usize>,
    artifact_path: Option<String>,
    note: Option<String>,
    chunk_path: Option<String>,
    report: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_scope: Option<String>,
}

#[derive(Serialize)]
struct MigrationNoteFingerprint {
    dispatch_index: Option<usize>,
    created_at_ms: i64,
    text: String,
}

impl FingerprintMaterial {
    fn from_bundle(bundle: &MigrationBundle) -> Self {
        let mut dispatches = bundle.dispatches.clone();
        dispatches.sort_by_key(|d| d.source_id);
        let indexes: BTreeMap<i64, usize> = dispatches
            .iter()
            .enumerate()
            .map(|(index, row)| (row.source_id, index))
            .collect();
        let mut notes = bundle.notes.clone();
        notes.sort_by_key(|n| (n.dispatch_id, n.created_at_ms, n.source_id));
        Self {
            session_name: bundle.session_name.clone(),
            worktree_path: bundle.worktree_path.clone(),
            groups: bundle
                .groups
                .iter()
                .map(|g| MigrationGroupFingerprint {
                    session_name: g.session_name.clone(),
                    name: g.name.clone(),
                    kind: g.kind.clone(),
                    worktree: g.worktree.clone(),
                    ordinal: g.ordinal,
                    active_tab: g.active_tab,
                    tabs: g
                        .tabs
                        .iter()
                        .cloned()
                        .map(|mut tab| {
                            tab.pane_sessions.clear();
                            tab
                        })
                        .collect(),
                })
                .collect(),
            ui_state: bundle.ui_state.clone(),
            dispatches: dispatches
                .iter()
                .map(|d| MigrationDispatchFingerprint {
                    issue_id: d.issue_id.clone(),
                    worktree_path: d.worktree_path.clone(),
                    agent_name: d.agent_name.clone(),
                    dispatched_at_ms: d.dispatched_at_ms,
                    status: d.status.clone(),
                    stage: d.stage.clone(),
                    parent_index: d.parent_id.and_then(|id| indexes.get(&id).copied()),
                    artifact_path: d.artifact_path.clone(),
                    note: d.note.clone(),
                    chunk_path: d.chunk_path.clone(),
                    report: d.report.clone(),
                    chunk_scope: d.chunk_scope.clone(),
                })
                .collect(),
            notes: notes
                .iter()
                .map(|n| MigrationNoteFingerprint {
                    dispatch_index: indexes.get(&n.dispatch_id).copied(),
                    created_at_ms: n.created_at_ms,
                    text: n.text.clone(),
                })
                .collect(),
            pin_state: bundle.pin_state.clone(),
        }
    }
}

/// Convert a persisted tab into its migration carrier.
pub fn migration_tab(session_name: &str, row: GroupTabRow) -> MigrationTab {
    MigrationTab {
        session_name: session_name.to_string(),
        group_name: row.group_name,
        ordinal: row.ordinal,
        title: row.title,
        pane_tree: row.pane_tree,
        focused_pane: row.focused_pane,
        pane_cwds: row.pane_cwds,
        pane_cmds: row.pane_cmds,
        pane_sessions: row.pane_sessions,
        scrollback_snapshot: row.scrollback_snapshot,
    }
}

/// Convert a migration tab back to the core persistence carrier.
pub fn persisted_tab(row: &MigrationTab) -> GroupTabRow {
    GroupTabRow {
        group_name: row.group_name.clone(),
        ordinal: row.ordinal,
        title: row.title.clone(),
        pane_tree: row.pane_tree.clone(),
        focused_pane: row.focused_pane,
        pane_cwds: row.pane_cwds.clone(),
        pane_cmds: row.pane_cmds.clone(),
        pane_sessions: String::new(),
        scrollback_snapshot: row.scrollback_snapshot.clone(),
    }
}

/// Convert a group carrier to the existing persistence carrier.
pub fn persisted_group(row: &MigrationGroup) -> TabGroupRow {
    TabGroupRow {
        name: row.name.clone(),
        kind: row.kind.clone(),
        worktree: row.worktree.clone(),
        ordinal: row.ordinal,
        active_tab: row.active_tab,
    }
}

/// Convert the allowlisted worktree carrier into the existing model type.
pub fn persisted_worktree(row: &MigrationWorktree) -> WorktreeRow {
    WorktreeRow {
        worktree: row.worktree.clone(),
        branch: row.branch.clone(),
        agent: row.agent.clone(),
        created_at: row.created_at,
        repo_root: row.repo_root.clone(),
        tab_name: row.tab_name.clone(),
        session_name: row.session_name.clone(),
        location: row.location.clone(),
        position: row.position,
        sandbox_backend: row.sandbox_backend.clone(),
        observed_backend: row.observed_backend.clone(),
        folder_id: row.folder_id,
        env_name: row.env_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str, path: &str, ordinal: i64) -> TabGroupRow {
        TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: path.into(),
            ordinal,
            active_tab: 0,
        }
    }

    fn tab(group_name: &str, ordinal: i64, session: &str) -> GroupTabRow {
        GroupTabRow {
            group_name: group_name.into(),
            ordinal,
            title: format!("tab-{ordinal}"),
            pane_tree: "{}".into(),
            focused_pane: 0,
            pane_cwds: "{}".into(),
            pane_cmds: "{}".into(),
            pane_sessions: format!("{session}-daemon-id"),
            scrollback_snapshot: "history".into(),
        }
    }

    fn bundle() -> MigrationBundle {
        let groups = select_groups(
            "sess",
            "/wt/api",
            &[group("api", "/wt/api", 2), group("api-v2", "/wt/api-v2", 1)],
            &[tab("api", 1, "source")],
        );
        make_bundle(
            "default",
            "work",
            "sess",
            "/wt/api",
            None,
            groups,
            vec![
                MigrationUiState {
                    scope: "sidebar".into(),
                    key: "pin:api".into(),
                    value: Some("1".into()),
                },
                MigrationUiState {
                    scope: "sidebar".into(),
                    key: "pin:api-v2".into(),
                    value: Some("2".into()),
                },
            ],
            Vec::new(),
            Vec::new(),
            Some("[\"api\"]".into()),
            Some(4),
        )
    }

    #[test]
    fn bundle_bounds_refuse_an_oversized_bundle() {
        let mut bundle = bundle();
        bundle.groups[0].tabs[0].scrollback_snapshot = "x".repeat(MAX_MIGRATION_BUNDLE_BYTES);

        assert!(matches!(
            check_bundle_bounds(&bundle),
            Err(MigrationBoundsError::TooLarge { bytes, max }) if bytes > max
        ));
    }

    #[test]
    fn bundle_bounds_refuse_deep_nesting_even_when_not_json() {
        let mut bundle = bundle();
        bundle.groups[0].tabs[0].pane_tree = "[".repeat(MAX_MIGRATION_JSON_DEPTH + 1);
        assert!(matches!(
            check_bundle_bounds(&bundle),
            Err(MigrationBoundsError::TooDeep {
                field: "pane_tree",
                ..
            })
        ));

        bundle.groups[0].tabs[0].pane_tree = format!(
            "{}{}",
            "[".repeat(MAX_MIGRATION_JSON_DEPTH),
            "]".repeat(MAX_MIGRATION_JSON_DEPTH)
        );
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));
    }

    #[test]
    fn bundle_bounds_refuse_duplicate_pane_session_keys() {
        let mut bundle = bundle();
        bundle.groups[0].tabs[0].pane_sessions =
            r#"{"1":{"session":"a"},"1":{"session":"b"}}"#.into();
        assert!(matches!(
            check_bundle_bounds(&bundle),
            Err(MigrationBoundsError::DuplicateKey { field: "pane_sessions", key, .. }) if key == "1"
        ));

        bundle.groups[0].tabs[0].pane_sessions.clear();
        bundle.groups[0].tabs[0].pane_cmds = r#"{"1":{"argv":[],"argv":[]}}"#.into();
        assert!(matches!(
            check_bundle_bounds(&bundle),
            Err(MigrationBoundsError::DuplicateKey { field: "pane_cmds", key, .. }) if key == "argv"
        ));
    }

    #[test]
    fn bundle_bounds_ignore_brackets_inside_strings_and_opaque_legacy_text() {
        let mut bundle = bundle();
        // Positive control: the fixture's legacy pane_sessions/scrollback stay opaque.
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));

        bundle.groups[0].tabs[0].pane_cwds = format!(r#"{{"path":"{}"}}"#, "[".repeat(100));
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));

        bundle.groups[0].tabs[0].pane_cwds = r#"{"quote":"\"[{}]","items":[{}]}"#.into();
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));

        bundle.groups[0].tabs[0].pane_cwds =
            r#"[true,false,null,-1,1,1.5,"text",{"nested":[]}]"#.into();
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));

        // An unterminated string ending in an escape remains opaque legacy text.
        bundle.groups[0].tabs[0].pane_cwds = r#"{"path":"abc\"#.into();
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));

        // Unmatched closing brackets in opaque text do not underflow the scanner.
        bundle.groups[0].tabs[0].pane_cwds = "]".into();
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));

        bundle.groups[0].tabs[0].pane_sessions =
            r#"{"1":{"session":"a"},"2":{"session":"b"}}"#.into();
        assert_eq!(check_bundle_bounds(&bundle), Ok(()));
    }

    #[test]
    fn migration_bounds_error_display_is_stable_and_duplicate_key_truncates_on_char_boundary() {
        assert_eq!(
            MigrationBoundsError::TooLarge { bytes: 5, max: 4 }.to_string(),
            "session bundle is 5 bytes; the limit is 4"
        );
        assert_eq!(
            MigrationBoundsError::TooDeep {
                group: "api".into(),
                ordinal: 2,
                field: "pane_tree",
                max: 32,
            }
            .to_string(),
            "group \"api\" tab 2 pane_tree nests deeper than 32"
        );

        let mut bundle = bundle();
        let key = "é".repeat(100);
        let encoded_key = serde_json::to_string(&key).unwrap();
        bundle.groups[0].tabs[0].pane_sessions = format!("{{{encoded_key}:1,{encoded_key}:2}}");
        let Err(error) = check_bundle_bounds(&bundle) else {
            panic!("duplicate pane session key should be refused");
        };
        let message = error.to_string();
        assert_eq!(message.matches('é').count(), 64);
    }

    #[test]
    fn exact_path_and_multiple_group_selection() {
        let groups = select_groups(
            "sess",
            "/wt/api",
            &[
                group("one", "/wt/api", 2),
                group("two", "/wt/api", 1),
                group("other", "/wt/no", 0),
            ],
            &[tab("one", 1, "s"), tab("two", 0, "s")],
        );
        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            ["two", "one"]
        );
        assert_eq!(groups[0].tabs[0].group_name, "two");
    }

    #[test]
    fn sidebar_matching_is_segment_anchored() {
        assert!(sidebar_key_matches("pin:api", "pin", "api"));
        assert!(sidebar_key_matches("pin:api/feature", "pin", "api"));
        assert!(!sidebar_key_matches("pin:api-v2", "pin", "api"));
        let state = select_sidebar_state(&bundle().ui_state, ["api"]);
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].key, "pin:api");
    }

    #[test]
    fn sanitizes_pane_sessions_and_fingerprint_is_stable() {
        let a = bundle();
        let mut b = a.clone();
        b.target_profile = "other".into();
        b.groups[0].tabs[0].pane_sessions = "another-daemon-id".into();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert!(!a.groups[0].tabs[0].pane_sessions.is_empty());
    }

    #[test]
    fn legacy_dispatch_bundle_defaults_chunk_scope_and_fingerprint_omits_it() {
        let mut current = bundle();
        current.dispatches.push(MigrationDispatch {
            source_id: 1,
            issue_id: "THE-214".into(),
            worktree_path: "/w".into(),
            agent_name: "agent".into(),
            dispatched_at_ms: 1,
            status: "running".into(),
            stage: None,
            parent_id: None,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: None,
            chunk_scope: None,
        });

        let legacy_json = serde_json::to_value(&current).unwrap();
        assert!(
            !legacy_json["dispatches"][0]
                .as_object()
                .unwrap()
                .contains_key("chunk_scope")
        );
        let legacy: MigrationBundle = serde_json::from_value(legacy_json).unwrap();

        assert_eq!(legacy.dispatches[0].chunk_scope, None);
        assert_eq!(legacy.fingerprint(), current.fingerprint());
        assert!(
            !serde_json::to_string(&FingerprintMaterial::from_bundle(&legacy))
                .unwrap()
                .contains("chunk_scope")
        );

        current.dispatches[0].chunk_scope = Some(r#"{"v":1,"files":["src/**"]}"#.into());
        assert_ne!(legacy.fingerprint(), current.fingerprint());
    }

    #[test]
    fn strict_conflicts_and_identical_resume() {
        let source = bundle();
        let mut target = MigrationTarget {
            groups: source.groups.clone(),
            ui_state: source.ui_state.clone(),
            pin_state: source.pin_state.clone(),
            ..Default::default()
        };
        let plan = plan_migration(source.clone(), target.clone(), HistoryPolicy::Include).unwrap();
        assert!(plan.resumed);

        target.ui_state[0].value = Some("different".into());
        assert_eq!(
            plan_migration(source, target, HistoryPolicy::Include),
            Err(MigrationConflict::UiState("pin:api".into()))
        );
    }

    #[test]
    fn target_owned_pin_is_preserved_when_source_has_no_pin() {
        let mut source = bundle();
        source.pin_state = None;
        let target = MigrationTarget {
            groups: source.groups.clone(),
            ui_state: source.ui_state.clone(),
            pin_state: Some("target-owned".into()),
            ..Default::default()
        };

        let plan = plan_migration(source, target, HistoryPolicy::Include).unwrap();
        assert!(plan.resumed);
    }

    #[test]
    fn differing_prior_dispatch_import_is_rejected() {
        let source = bundle();
        let mut target = MigrationTarget {
            groups: source.groups.clone(),
            ..Default::default()
        };
        target.dispatches.push(MigrationDispatch {
            source_id: 50,
            issue_id: "different".into(),
            worktree_path: "/wt/api".into(),
            agent_name: "agent".into(),
            dispatched_at_ms: 1,
            status: "queued".into(),
            stage: None,
            parent_id: None,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: None,
            chunk_scope: None,
        });
        assert_eq!(
            plan_migration(source, target, HistoryPolicy::Include),
            Err(MigrationConflict::PriorImport)
        );
    }

    #[test]
    fn dispatch_remap_keeps_only_in_set_parent() {
        let rows = vec![
            MigrationDispatch {
                source_id: 7,
                issue_id: "a".into(),
                worktree_path: "/w".into(),
                agent_name: "a".into(),
                dispatched_at_ms: 1,
                status: "queued".into(),
                stage: None,
                parent_id: Some(99),
                session_id: Some("s".into()),
                artifact_path: None,
                note: None,
                chunk_path: None,
                report: None,
                chunk_scope: None,
            },
            MigrationDispatch {
                source_id: 9,
                issue_id: "b".into(),
                worktree_path: "/w".into(),
                agent_name: "b".into(),
                dispatched_at_ms: 2,
                status: "done".into(),
                stage: None,
                parent_id: Some(7),
                session_id: Some("t".into()),
                artifact_path: None,
                note: None,
                chunk_path: None,
                report: None,
                chunk_scope: None,
            },
        ];
        let map = BTreeMap::from([(7, 101), (9, 102)]);
        let out = remap_dispatches(&rows, &map);
        assert_eq!(out[0].source_id, 101);
        assert_eq!(out[0].parent_id, None);
        assert_eq!(out[1].parent_id, Some(101));
        assert!(out.iter().all(|r| r.session_id.is_none()));
    }

    #[test]
    fn imported_dispatch_parks_active_rows_and_clears_locators() {
        for status in ["queued", "spawning", "running", "waiting_human", "pr_open"] {
            let row = MigrationDispatch {
                source_id: 7,
                issue_id: "THE-1144".into(),
                worktree_path: "/source/wt".into(),
                agent_name: "agent".into(),
                dispatched_at_ms: 42,
                status: status.into(),
                stage: Some("review".into()),
                parent_id: Some(2),
                session_id: Some("source-session".into()),
                artifact_path: Some("/source/artifact.md".into()),
                note: Some("old note".into()),
                chunk_path: Some("/source/chunk.md".into()),
                report: Some("report".into()),
                chunk_scope: Some("scope".into()),
            };

            let imported = imported_dispatch(&row);
            assert_eq!(imported.status, "abandoned");
            assert_eq!(imported.session_id, None);
            assert_eq!(imported.artifact_path, None);
            assert_eq!(imported.chunk_path, None);
            assert!(
                imported
                    .note
                    .as_deref()
                    .unwrap()
                    .starts_with(&format!("[migrated while {status}: not resumed]"))
            );
            assert!(imported.note.as_deref().unwrap().ends_with("old note"));
            assert_eq!(imported.stage, row.stage);
            assert_eq!(imported.report, row.report);
            assert_eq!(imported.chunk_scope, row.chunk_scope);
        }
    }

    #[test]
    fn imported_dispatch_keeps_terminal_and_unknown_status_and_is_idempotent() {
        for status in ["done", "merged", "abandoned", "failed", "future_state"] {
            let row = MigrationDispatch {
                source_id: 9,
                issue_id: "THE-1144".into(),
                worktree_path: "/source/wt".into(),
                agent_name: "agent".into(),
                dispatched_at_ms: 42,
                status: status.into(),
                stage: None,
                parent_id: None,
                session_id: Some("source-session".into()),
                artifact_path: Some("/source/artifact.md".into()),
                note: Some("keep this note".into()),
                chunk_path: Some("/source/chunk.md".into()),
                report: None,
                chunk_scope: None,
            };

            let imported = imported_dispatch(&row);
            assert_eq!(imported.status, status);
            assert_eq!(imported.note, row.note);
            assert_eq!(imported.session_id, None);
            assert_eq!(imported.artifact_path, None);
            assert_eq!(imported.chunk_path, None);
            assert_eq!(imported_dispatch(&imported), imported);
        }
    }

    #[test]
    fn plan_fingerprint_covers_imported_rows_and_refuses_a_raw_prior_copy() {
        let mut source = bundle();
        source.dispatches.push(MigrationDispatch {
            source_id: 11,
            issue_id: "THE-1144".into(),
            worktree_path: source.worktree_path.clone(),
            agent_name: "agent".into(),
            dispatched_at_ms: 42,
            status: "running".into(),
            stage: None,
            parent_id: None,
            session_id: Some("source-session".into()),
            artifact_path: Some("/source/artifact.md".into()),
            note: Some("working".into()),
            chunk_path: Some("/source/chunk.md".into()),
            report: None,
            chunk_scope: None,
        });
        let mut imported_target = MigrationTarget {
            groups: source.groups.clone(),
            ui_state: source.ui_state.clone(),
            pin_state: source.pin_state.clone(),
            ..Default::default()
        };
        imported_target.dispatches = imported_bundle(&source, HistoryPolicy::Include).dispatches;
        let plan = plan_migration(source.clone(), imported_target, HistoryPolicy::Include).unwrap();
        assert!(plan.resumed);

        let mut raw_target = MigrationTarget {
            groups: source.groups.clone(),
            ui_state: source.ui_state.clone(),
            pin_state: source.pin_state.clone(),
            ..Default::default()
        };
        raw_target.dispatches = source.dispatches.clone();
        assert_eq!(
            plan_migration(source, raw_target, HistoryPolicy::Include),
            Err(MigrationConflict::PriorImport)
        );
    }

    #[test]
    fn history_drop_empties_scrollback_commands_reports_and_notes() {
        let mut source = bundle();
        source.groups[0].tabs[0].scrollback_snapshot = "SECRET_SCROLLBACK".into();
        source.groups[0].tabs[0].pane_cmds = "SECRET_COMMAND".into();
        source.dispatches = vec![
            MigrationDispatch {
                source_id: 1,
                issue_id: "running".into(),
                worktree_path: source.worktree_path.clone(),
                agent_name: "agent".into(),
                dispatched_at_ms: 1,
                status: "running".into(),
                stage: None,
                parent_id: None,
                session_id: None,
                artifact_path: None,
                note: Some("SECRET_NOTE".into()),
                chunk_path: None,
                report: Some("SECRET_REPORT".into()),
                chunk_scope: None,
            },
            MigrationDispatch {
                source_id: 2,
                issue_id: "done".into(),
                worktree_path: source.worktree_path.clone(),
                agent_name: "agent".into(),
                dispatched_at_ms: 2,
                status: "done".into(),
                stage: None,
                parent_id: None,
                session_id: None,
                artifact_path: None,
                note: Some("SECRET_DONE_NOTE".into()),
                chunk_path: None,
                report: Some("SECRET_DONE_REPORT".into()),
                chunk_scope: None,
            },
            MigrationDispatch {
                source_id: 3,
                issue_id: "abandoned".into(),
                worktree_path: source.worktree_path.clone(),
                agent_name: "agent".into(),
                dispatched_at_ms: 3,
                status: "abandoned".into(),
                stage: None,
                parent_id: None,
                session_id: None,
                artifact_path: None,
                note: Some("SECRET_ABANDONED_NOTE".into()),
                chunk_path: None,
                report: Some("SECRET_ABANDONED_REPORT".into()),
                chunk_scope: None,
            },
        ];
        source.notes.push(MigrationNote {
            source_id: 3,
            dispatch_id: 1,
            created_at_ms: 3,
            text: "SECRET_TEXT".into(),
        });

        let imported = imported_bundle(&source, HistoryPolicy::Drop);
        assert!(imported.groups[0].tabs[0].scrollback_snapshot.is_empty());
        assert!(imported.groups[0].tabs[0].pane_cmds.is_empty());
        assert_eq!(imported.dispatches[0].report, None);
        assert_eq!(imported.dispatches[1].report, None);
        assert_eq!(imported.dispatches[2].report, None);
        assert_eq!(
            imported.dispatches[0].note.as_deref(),
            Some("[migrated while running: not resumed]")
        );
        assert_eq!(imported.dispatches[1].note, None);
        assert_eq!(imported.dispatches[2].note, None);
        assert!(imported.notes.is_empty());
        assert!(
            !serde_json::to_string(&imported)
                .unwrap()
                .contains("SECRET_")
        );
    }

    #[test]
    fn history_include_keeps_todays_imported_form_and_drop_is_idempotent() {
        let mut source = bundle();
        source.groups[0].tabs[0].scrollback_snapshot = "SECRET_SCROLLBACK".into();
        source.groups[0].tabs[0].pane_cmds = "SECRET_COMMAND".into();
        source.dispatches.push(MigrationDispatch {
            source_id: 1,
            issue_id: "running".into(),
            worktree_path: source.worktree_path.clone(),
            agent_name: "agent".into(),
            dispatched_at_ms: 1,
            status: "running".into(),
            stage: None,
            parent_id: None,
            session_id: Some("SECRET_SESSION".into()),
            artifact_path: Some("SECRET_ARTIFACT".into()),
            note: Some("SECRET_NOTE".into()),
            chunk_path: Some("SECRET_CHUNK".into()),
            report: Some("SECRET_REPORT".into()),
            chunk_scope: None,
        });
        source.notes.push(MigrationNote {
            source_id: 2,
            dispatch_id: 1,
            created_at_ms: 2,
            text: "SECRET_TEXT".into(),
        });

        let included = imported_bundle(&source, HistoryPolicy::Include);
        assert_eq!(
            included.groups[0].tabs[0].scrollback_snapshot,
            "SECRET_SCROLLBACK"
        );
        assert_eq!(included.groups[0].tabs[0].pane_cmds, "SECRET_COMMAND");
        assert_eq!(
            included.dispatches[0].note.as_deref(),
            Some("[migrated while running: not resumed] SECRET_NOTE")
        );
        assert_eq!(
            included.dispatches[0].report.as_deref(),
            Some("SECRET_REPORT")
        );
        assert_eq!(included.notes[0].text, "SECRET_TEXT");
        assert_eq!(included.dispatches[0].session_id, None);
        assert_eq!(included.dispatches[0].artifact_path, None);
        assert_eq!(included.dispatches[0].chunk_path, None);

        let dropped = imported_bundle(&source, HistoryPolicy::Drop);
        assert_eq!(imported_bundle(&dropped, HistoryPolicy::Drop), dropped);
    }

    #[test]
    fn pane_cwds_outside_the_worktree_are_dropped_on_import() {
        let mut source = bundle();
        source.groups[0].tabs[0].pane_cwds =
            r#"{"1":"/wt/api/src","2":"/home/u/.config/work","3":"/wt/api-v2","4":"/wt/api"}"#
                .into();

        for history in [HistoryPolicy::Include, HistoryPolicy::Drop] {
            let imported = imported_bundle(&source, history);
            let cwds: BTreeMap<u32, String> =
                serde_json::from_str(&imported.groups[0].tabs[0].pane_cwds).unwrap();
            assert_eq!(
                cwds,
                BTreeMap::from([(1, "/wt/api/src".into()), (4, "/wt/api".into())])
            );
        }
    }

    #[test]
    fn pane_cwds_unparsable_or_relative_hints_are_dropped() {
        assert_eq!(scoped_pane_cwds("not json", "/wt/api"), "");
        assert_eq!(scoped_pane_cwds(r#"{"1":"src"}"#, "/wt/api"), "{}");
        assert_eq!(scoped_pane_cwds(r#"{"1":"/wt/api/src"}"#, ""), "{}");
        assert_eq!(scoped_pane_cwds("", "/wt/api"), "");
    }

    #[test]
    fn drop_plan_resumes_against_its_own_import_and_refuses_a_with_history_target() {
        let source = bundle();
        let dropped = imported_bundle(&source, HistoryPolicy::Drop);
        let drop_target = MigrationTarget {
            groups: dropped.groups.clone(),
            ui_state: dropped.ui_state.clone(),
            pin_state: dropped.pin_state.clone(),
            dispatches: dropped.dispatches.clone(),
            ..Default::default()
        };
        let plan = plan_migration(source.clone(), drop_target, HistoryPolicy::Drop).unwrap();
        assert!(plan.resumed);

        let include_target = MigrationTarget {
            groups: source.groups.clone(),
            ui_state: source.ui_state.clone(),
            pin_state: source.pin_state.clone(),
            dispatches: imported_bundle(&source, HistoryPolicy::Include).dispatches,
            ..Default::default()
        };
        assert!(matches!(
            plan_migration(source, include_target, HistoryPolicy::Drop),
            Err(MigrationConflict::Group(_))
        ));
    }
}
