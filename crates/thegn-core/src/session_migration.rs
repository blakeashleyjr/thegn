//! Pure policy and row carriers for moving a persisted session presentation
//! between profile databases.
//!
//! This module deliberately knows nothing about terminals, daemons, config, or
//! credentials. JSON and command fields remain opaque; the target compositor is
//! responsible for interpreting them after resurrection.

use crate::issue::{AgentDispatch, AgentDispatchStatus, DispatchNote};
use crate::models::{GroupTabRow, TabGroupRow, WorktreeRow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

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

/// An explicitly allowlisted tab payload. `pane_sessions` is retained in the
/// source snapshot for liveness preflight, then cleared at import because daemon
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
    pub status: AgentDispatchStatus,
    pub stage: Option<String>,
    pub parent_id: Option<i64>,
    pub session_id: Option<String>,
    pub artifact_path: Option<String>,
    pub note: Option<String>,
    pub chunk_path: Option<String>,
    pub report: Option<String>,
}

impl From<AgentDispatch> for MigrationDispatch {
    fn from(row: AgentDispatch) -> Self {
        Self {
            source_id: row.id,
            issue_id: row.issue_id,
            worktree_path: row.worktree_path,
            agent_name: row.agent_name,
            dispatched_at_ms: row.dispatched_at_ms,
            status: row.status,
            stage: row.stage,
            parent_id: row.parent_id,
            session_id: row.session_id,
            artifact_path: row.artifact_path,
            note: row.note,
            chunk_path: row.chunk_path,
            report: row.report,
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
/// or the one permitted session-state field; credentials and global caches have
/// no carrier by construction.
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

/// A pure, validated import decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    pub bundle: MigrationBundle,
    pub target: MigrationTarget,
    pub fingerprint: String,
    pub resumed: bool,
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
pub fn plan_migration(
    bundle: MigrationBundle,
    target: MigrationTarget,
) -> Result<MigrationPlan, MigrationConflict> {
    for source_group in &bundle.groups {
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
    let fingerprint = bundle.fingerprint();
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
        target.pin_state.clone(),
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

/// A stable SHA-256 over sanitized rows. Database ids, daemon ids, profile
/// names, and target-owned worktree metadata are intentionally not included.
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
    status: AgentDispatchStatus,
    stage: Option<String>,
    parent_index: Option<usize>,
    artifact_path: Option<String>,
    note: Option<String>,
    chunk_path: Option<String>,
    report: Option<String>,
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
                    status: d.status,
                    stage: d.stage.clone(),
                    parent_index: d.parent_id.and_then(|id| indexes.get(&id).copied()),
                    artifact_path: d.artifact_path.clone(),
                    note: d.note.clone(),
                    chunk_path: d.chunk_path.clone(),
                    report: d.report.clone(),
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
    fn strict_conflicts_and_identical_resume() {
        let source = bundle();
        let mut target = MigrationTarget {
            groups: source.groups.clone(),
            ui_state: source.ui_state.clone(),
            pin_state: source.pin_state.clone(),
            ..Default::default()
        };
        let plan = plan_migration(source.clone(), target.clone()).unwrap();
        assert!(plan.resumed);

        target.ui_state[0].value = Some("different".into());
        assert_eq!(
            plan_migration(source, target),
            Err(MigrationConflict::UiState("pin:api".into()))
        );
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
            status: AgentDispatchStatus::Queued,
            stage: None,
            parent_id: None,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: None,
        });
        assert_eq!(
            plan_migration(source, target),
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
                status: AgentDispatchStatus::Queued,
                stage: None,
                parent_id: Some(99),
                session_id: Some("s".into()),
                artifact_path: None,
                note: None,
                chunk_path: None,
                report: None,
            },
            MigrationDispatch {
                source_id: 9,
                issue_id: "b".into(),
                worktree_path: "/w".into(),
                agent_name: "b".into(),
                dispatched_at_ms: 2,
                status: AgentDispatchStatus::Done,
                stage: None,
                parent_id: Some(7),
                session_id: Some("t".into()),
                artifact_path: None,
                note: None,
                chunk_path: None,
                report: None,
            },
        ];
        let map = BTreeMap::from([(7, 101), (9, 102)]);
        let out = remap_dispatches(&rows, &map);
        assert_eq!(out[0].source_id, 101);
        assert_eq!(out[0].parent_id, None);
        assert_eq!(out[1].parent_id, Some(101));
        assert!(out.iter().all(|r| r.session_id.is_none()));
    }
}
