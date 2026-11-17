//! Palette zone assignment (`zone-assign`): pick a zone for the active
//! workspace, confirm, then persist off the loop.
//!
//! Zones only clamp (egress intersects down, the sandbox floor only rises), so
//! assigning an unzoned workspace can only narrow what new panes get. Removing
//! a workspace from its zone, or moving it between zones, may widen ceilings:
//! those confirms default to cancel. The picker reads the hydrated
//! `model.zone_catalog` (no DB read on the loop); the write is
//! [`crate::db_task::persist_critical`]. CLI parity: `thegn zone assign`.

use crate::chrome::ZoneCatalog;
use crate::menu::{MenuOverlay, ZONE_NONE_ARG};
use std::path::{Path, PathBuf};

pub(crate) const NEEDS_REPO: &str = "zone assign needs a repository workspace";
pub(crate) const UNREADABLE: &str = "zones could not be read; try thegn zone list";
pub(crate) const NO_ZONES: &str = "no zones yet: thegn zone create <name>";

/// What a zone change can do to a workspace's ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignPlan {
    /// Already there: nothing to write.
    Noop,
    /// Unzoned to a zone: ceilings can only tighten.
    Narrowing,
    /// Unassign, or zone A to zone B: ceilings may loosen.
    MayWiden,
}

pub(crate) fn assign_plan(current: Option<&str>, target: Option<&str>) -> AssignPlan {
    match (current, target) {
        (current, target) if current == target => AssignPlan::Noop,
        (None, Some(_)) => AssignPlan::Narrowing,
        _ => AssignPlan::MayWiden,
    }
}

/// Menu arg encoding: the zone name, or `-` alone for "no zone".
pub(crate) fn encode_arg(target: Option<&str>) -> String {
    target.unwrap_or(ZONE_NONE_ARG).to_string()
}

pub(crate) fn decode_arg(arg: &str) -> Option<String> {
    (arg != ZONE_NONE_ARG).then(|| arg.to_string())
}

/// The workspace a zone change targets: the active session's repo root plus
/// the label the sidebar shows for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZoneTarget {
    pub(crate) root: PathBuf,
    pub(crate) label: String,
}

impl ZoneTarget {
    fn key(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

/// Resolve the active workspace from loop-safe state only (no git, no DB):
/// the session's repo root and the hydrated `(slug, display, kind, repo_path)`
/// sidebar list. The home directory and plain-dir workspaces are refused.
pub(crate) fn target_for(
    repo_root: Option<&Path>,
    workspaces: &[(String, String, String, String)],
    home: Option<&Path>,
) -> Option<ZoneTarget> {
    let root = repo_root?;
    if home.is_some_and(|home| home == root) {
        return None;
    }
    let key = root.to_string_lossy();
    let row = workspaces
        .iter()
        .find(|(_, _, _, repo_path)| repo_path.as_str() == key);
    if row.is_some_and(|(_, _, kind, _)| kind == "dir") {
        return None;
    }
    let label = row
        .map(|(_, display, _, _)| display.clone())
        .filter(|display| !display.is_empty())
        .or_else(|| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| key.clone().into_owned());
    Some(ZoneTarget {
        root: root.to_path_buf(),
        label,
    })
}

/// The user's home directory, for [`target_for`]'s home refusal.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The picker for the active workspace's current zone.
pub(crate) fn pick_menu(catalog: &ZoneCatalog, current: Option<&str>) -> MenuOverlay {
    crate::menu::zone_pick_menu(&catalog.names, current)
}

/// `Action::ZoneAssign`: the picker, or the status line explaining why not.
pub(crate) fn open_picker(
    target: Option<&ZoneTarget>,
    catalog: &ZoneCatalog,
) -> Result<MenuOverlay, String> {
    let Some(target) = target else {
        return Err(NEEDS_REPO.to_string());
    };
    if catalog.read_failed {
        return Err(UNREADABLE.to_string());
    }
    if catalog.names.is_empty() {
        return Err(NO_ZONES.to_string());
    }
    let current = catalog.by_repo.get(&target.key()).map(String::as_str);
    Ok(pick_menu(catalog, current))
}

pub(crate) fn noop_status(target: Option<&str>) -> String {
    match target {
        Some(zone) => format!("already in zone {zone}"),
        None => "not in a zone".to_string(),
    }
}

/// The confirm for a planned change; `None` for [`AssignPlan::Noop`].
pub(crate) fn confirm_for(
    repo_label: &str,
    current: Option<&str>,
    plan: AssignPlan,
    target: Option<&str>,
) -> Option<MenuOverlay> {
    let arg = encode_arg(target);
    match (plan, current, target) {
        (AssignPlan::Noop, _, _) => None,
        (AssignPlan::Narrowing, _, Some(zone)) => Some(crate::menu::zone_assign_confirm_menu(
            format!("Assign {repo_label} to zone {zone}?"),
            format!(
                "Zone {zone}'s egress ceiling, sandbox floor and credential sub-vault apply to new panes."
            ),
            arg,
            false,
        )),
        (_, Some(from), None) => Some(crate::menu::zone_assign_confirm_menu(
            format!("Remove {repo_label} from zone {from}?"),
            format!(
                "Zone {from}'s egress ceiling, sandbox floor and credential sub-vault are removed for new panes; limits may widen."
            ),
            arg,
            true,
        )),
        (_, Some(from), Some(zone)) => Some(crate::menu::zone_assign_confirm_menu(
            format!("Move {repo_label} from zone {from} to {zone}?"),
            format!(
                "Zone {from}'s ceilings are replaced by zone {zone}'s for new panes; egress and credentials may widen."
            ),
            arg,
            true,
        )),
        // Narrowing always has a target and MayWiden always has a current
        // zone (see `assign_plan`); treat anything else as a no-op.
        _ => None,
    }
}

/// The `zone-pick` confirm: the next confirm menu, or a status line.
pub(crate) fn after_pick(
    target: &ZoneTarget,
    catalog: &ZoneCatalog,
    arg: &str,
) -> Result<MenuOverlay, String> {
    let current = catalog.by_repo.get(&target.key()).map(String::as_str);
    let picked = decode_arg(arg);
    let plan = assign_plan(current, picked.as_deref());
    confirm_for(&target.label, current, plan, picked.as_deref())
        .ok_or_else(|| noop_status(picked.as_deref()))
}

/// The DB half of an assignment, run on the writer thread. The zone id is
/// looked up by name here (never carried from the picker), so a zone deleted
/// since the catalog was hydrated is refused instead of dangling. The lookup
/// and the write run in one immediate transaction, so a concurrent CLI delete
/// (and a recycled zone id) cannot slip in between them. A repo with
/// no `workspaces` row gets one first (the CLI does the same), without
/// renaming an existing row. Returns the alert text on failure.
pub(crate) fn write_assignment(
    db: &thegn_core::db::Db,
    repo_path: &str,
    label: &str,
    target: Option<&str>,
) -> Result<(), String> {
    use thegn_core::store::{WorkspaceStore, ZoneStore};
    let Some(zone) = target else {
        return db
            .assign_workspace_zone(repo_path, None)
            .map_err(|error| format!("Could not remove {label} from its zone: {error}"));
    };
    let assigned = db
        .transaction_immediate(|db| {
            let Some(zone_id) = db
                .list_zones()?
                .into_iter()
                .find(|row| row.name == zone)
                .map(|row| row.zone_id)
            else {
                return Ok(false);
            };
            let known = db
                .workspaces()?
                .iter()
                .any(|row| row.repo_path == repo_path);
            if !known {
                db.put_workspace(repo_path, label, "repo")?;
            }
            db.assign_workspace_zone(repo_path, Some(zone_id))?;
            Ok(true)
        })
        .map_err(|error| format!("Could not assign {label} to zone {zone}: {error}"))?;
    if assigned {
        Ok(())
    } else {
        Err(format!("zone {zone} no longer exists"))
    }
}

/// The `zone-assign` confirm: queue the write, then a model refresh so the
/// sidebar chip and catalog catch up. Returns the status line.
pub(crate) fn apply(
    target: &ZoneTarget,
    zone: Option<String>,
    refresh_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
) -> String {
    let status = match &zone {
        Some(zone) => format!(
            "Assigning {} to zone {zone}; applies to new panes",
            target.label
        ),
        None => format!(
            "Removing {} from its zone; applies to new panes",
            target.label
        ),
    };
    let repo_path = target.key();
    let label = target.label.clone();
    crate::db_task::persist_critical(move |db| {
        if let Err(message) = write_assignment(db, &repo_path, &label, zone.as_deref()) {
            tracing::warn!(target: "thegn::zone", repo = %repo_path, %message, "zone assignment failed");
            crate::db_task::alert(&message);
            return;
        }
        // best-effort: the refresh only speeds up the chip; the next hydration
        // pass reads the same rows.
        if let Err(error) = refresh_tx.send(crate::hydrate::RefreshKind::Model) {
            tracing::debug!(target: "thegn::hydrate", %error, "model refresh request failed after zone assignment");
        }
    });
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::MenuChoice;
    use thegn_core::store::{WorkspaceStore, ZoneStore};

    fn catalog(names: &[&str], by_repo: &[(&str, &str)]) -> ZoneCatalog {
        ZoneCatalog {
            names: names.iter().map(|n| n.to_string()).collect(),
            by_repo: by_repo
                .iter()
                .map(|(r, z)| (r.to_string(), z.to_string()))
                .collect(),
            read_failed: false,
        }
    }

    fn target(root: &str) -> ZoneTarget {
        ZoneTarget {
            root: PathBuf::from(root),
            label: "repo".into(),
        }
    }

    #[test]
    fn zone_assign_plan_table() {
        assert_eq!(assign_plan(None, Some("z")), AssignPlan::Narrowing);
        assert_eq!(assign_plan(Some("z"), None), AssignPlan::MayWiden);
        assert_eq!(assign_plan(Some("a"), Some("b")), AssignPlan::MayWiden);
        assert_eq!(assign_plan(Some("z"), Some("z")), AssignPlan::Noop);
        assert_eq!(assign_plan(None, None), AssignPlan::Noop);
    }

    #[test]
    fn zone_assign_arg_round_trips_with_dash_alone_as_none() {
        assert_eq!(encode_arg(None), "-");
        assert_eq!(decode_arg("-"), None);
        for name in ["z1", "team-a", "-x", "x-", "--"] {
            assert_eq!(decode_arg(&encode_arg(Some(name))).as_deref(), Some(name));
        }
    }

    #[test]
    fn zone_assign_target_refuses_home_and_dir_workspaces() {
        let ws = vec![
            ("r".into(), "Repo".into(), "repo".into(), "/w/repo".into()),
            ("d".into(), "Dir".into(), "dir".into(), "/w/dir".into()),
        ];
        let home = Path::new("/home/u");
        assert_eq!(target_for(None, &ws, Some(home)), None);
        assert_eq!(target_for(Some(home), &ws, Some(home)), None);
        assert_eq!(target_for(Some(Path::new("/w/dir")), &ws, Some(home)), None);
        let t = target_for(Some(Path::new("/w/repo")), &ws, Some(home)).unwrap();
        assert_eq!(t.label, "Repo");
        let fallback = target_for(Some(Path::new("/w/other")), &ws, Some(home)).unwrap();
        assert_eq!(fallback.label, "other");
    }

    #[test]
    fn zone_assign_open_picker_statuses() {
        let t = target("/w/repo");
        assert_eq!(
            open_picker(None, &catalog(&["z"], &[])).unwrap_err(),
            NEEDS_REPO
        );
        let mut failed = catalog(&["z"], &[]);
        failed.read_failed = true;
        assert_eq!(open_picker(Some(&t), &failed).unwrap_err(), UNREADABLE);
        assert_eq!(
            open_picker(Some(&t), &catalog(&[], &[])).unwrap_err(),
            NO_ZONES
        );

        let unzoned = open_picker(Some(&t), &catalog(&["z1", "z2"], &[])).unwrap();
        assert!(unzoned.items().iter().all(|i| i.label != "(none)"));
        let zoned = open_picker(Some(&t), &catalog(&["z1", "z2"], &[("/w/repo", "z2")])).unwrap();
        assert_eq!(zoned.items().last().unwrap().label, "(none)");
        assert_eq!(zoned.items()[zoned.selected()].label, "z2");
    }

    #[test]
    fn zone_assign_after_pick_opens_the_right_confirm() {
        let t = target("/w/repo");
        let zoned = catalog(&["a", "b"], &[("/w/repo", "a")]);
        let unzoned = catalog(&["a", "b"], &[]);

        assert_eq!(
            after_pick(&t, &zoned, "a").unwrap_err(),
            "already in zone a"
        );

        let narrow = after_pick(&t, &unzoned, "a").unwrap();
        assert_eq!(
            narrow.items()[narrow.selected()].choice,
            MenuChoice::Confirm {
                tag: "zone-assign",
                arg: "a".into()
            }
        );

        for arg in ["-", "b"] {
            let widen = after_pick(&t, &zoned, arg).unwrap();
            assert_eq!(
                widen.items()[widen.selected()].choice,
                MenuChoice::Dismiss,
                "{arg}: may-widen confirm must default to cancel"
            );
            assert!(widen.title.contains("zone a"), "{}", widen.title);
        }
    }

    #[test]
    fn zone_assign_confirm_for_noop_is_none() {
        assert!(confirm_for("repo", Some("z"), AssignPlan::Noop, Some("z")).is_none());
    }

    #[test]
    fn zone_assign_write_assigns_unassigns_and_creates_the_workspace_row() {
        let _state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        db.create_zone("z1", 1).unwrap();
        let repo = "/w/new-repo";
        assert!(
            !db.workspaces().unwrap().iter().any(|w| w.repo_path == repo),
            "fixture starts with no workspaces row"
        );

        write_assignment(&db, repo, "new-repo", Some("z1")).unwrap();
        let row = db.zone_of_workspace(repo).unwrap().expect("assigned");
        assert_eq!(row.name, "z1");
        let ws = db.workspaces().unwrap();
        let ws = ws
            .iter()
            .find(|w| w.repo_path == repo)
            .expect("row created");
        assert_eq!(ws.name, "new-repo");

        write_assignment(&db, repo, "new-repo", None).unwrap();
        assert_eq!(db.zone_of_workspace(repo).unwrap(), None);
    }

    #[test]
    fn zone_assign_write_keeps_an_existing_workspace_name() {
        let _state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        db.create_zone("z1", 1).unwrap();
        db.put_workspace("/w/r", "Custom", "repo").unwrap();
        write_assignment(&db, "/w/r", "r", Some("z1")).unwrap();
        let ws = db.workspaces().unwrap();
        assert_eq!(
            ws.iter().find(|w| w.repo_path == "/w/r").unwrap().name,
            "Custom"
        );
    }

    #[test]
    fn zone_assign_write_refuses_a_vanished_zone() {
        let _state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        let err = write_assignment(&db, "/w/r", "r", Some("gone")).unwrap_err();
        assert_eq!(err, "zone gone no longer exists");
        assert!(
            !db.workspaces()
                .unwrap()
                .iter()
                .any(|w| w.repo_path == "/w/r"),
            "a refused assignment must not register the workspace"
        );
    }

    #[test]
    fn zone_assign_apply_persists_and_requests_a_model_refresh() {
        let _state = crate::testenv::IsolatedState::new();
        {
            let db = thegn_core::db::Db::open().unwrap();
            db.create_zone("z1", 1).unwrap();
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let t = target("/w/apply-repo");
        let status = apply(&t, Some("z1".into()), tx);
        assert_eq!(status, "Assigning repo to zone z1; applies to new panes");
        assert!(crate::db_task::flush(std::time::Duration::from_secs(10)));
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::hydrate::RefreshKind::Model)
        ));
        let db = thegn_core::db::Db::open().unwrap();
        assert_eq!(
            db.zone_of_workspace("/w/apply-repo")
                .unwrap()
                .map(|z| z.name)
                .as_deref(),
            Some("z1")
        );
    }
}
