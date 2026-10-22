//! Plans masks for the daemon control socket when a sandbox bind exposes it.
//!
//! The daemon socket confers host control authority. Bwrap can hide a visible
//! socket directory after its binds; fixed-mount container backends must refuse.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PUBLISHED: OnceLock<PathBuf> = OnceLock::new();

/// Publish the resolved host control-socket directory once during startup.
pub fn publish_control_socket_dir(dir: PathBuf) {
    PUBLISHED.get_or_init(|| dir);
}

pub(crate) fn control_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())
    {
        dirs.push(PathBuf::from(runtime_dir).join("thegn"));
    }
    if let Some(dir) = PUBLISHED.get() {
        dirs.push(dir.clone());
    }
    normalize(dirs)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlExposure {
    pub control_dir: PathBuf,
    pub visible_through: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MaskPlan {
    Clear,
    Mask(Vec<PathBuf>),
    Refuse(ControlExposure),
}

pub(crate) fn plan(
    control_dirs: &[PathBuf],
    visible_roots: &[&Path],
    bind_dests: &[&Path],
    is_dir: &dyn Fn(&Path) -> bool,
) -> MaskPlan {
    let mut masks = Vec::new();
    for control_dir in control_dirs {
        let Some(visible_through) = visible_roots
            .iter()
            .filter(|root| control_dir.starts_with(**root))
            .max_by_key(|root| root.components().count())
            .copied()
        else {
            continue;
        };

        let mut ancestor = control_dir.as_path();
        let mut mask = None;
        while ancestor.starts_with(visible_through) && ancestor != visible_through {
            if is_dir(ancestor) {
                mask = Some(ancestor.to_path_buf());
                break;
            }
            let Some(parent) = ancestor.parent() else {
                break;
            };
            ancestor = parent;
        }
        let Some(mask) = mask else {
            return MaskPlan::Refuse(ControlExposure {
                control_dir: control_dir.clone(),
                visible_through: visible_through.to_path_buf(),
            });
        };
        if bind_dests.iter().any(|dest| dest.starts_with(&mask)) {
            return MaskPlan::Refuse(ControlExposure {
                control_dir: control_dir.clone(),
                visible_through: visible_through.to_path_buf(),
            });
        }
        if !masks.contains(&mask) {
            masks.push(mask);
        }
    }
    if masks.is_empty() {
        MaskPlan::Clear
    } else {
        MaskPlan::Mask(masks)
    }
}

/// Translate host control directories into the destination space of emitted binds.
/// A failed canonicalization that could lexically match a bind is returned as an
/// exposure so callers fail closed instead of guessing which path the bind shows.
pub(crate) fn in_sandbox_paths(
    control_dirs: &[PathBuf],
    mounts: &[(&Path, &Path)],
) -> (Vec<PathBuf>, Option<ControlExposure>) {
    let mut translated = Vec::new();
    for control_dir in control_dirs {
        let canonical_control = canonicalize_with_missing_tail(control_dir);
        for (host, dest) in mounts {
            let canonical_host = std::fs::canonicalize(host);
            let Some(control_path) = canonical_control.as_ref() else {
                if control_dir.starts_with(host) {
                    return (
                        translated,
                        Some(ControlExposure {
                            control_dir: control_dir.clone(),
                            visible_through: (*dest).to_path_buf(),
                        }),
                    );
                }
                continue;
            };
            let Ok(host_path) = canonical_host else {
                if control_dir.starts_with(host) {
                    return (
                        translated,
                        Some(ControlExposure {
                            control_dir: control_dir.clone(),
                            visible_through: (*dest).to_path_buf(),
                        }),
                    );
                }
                continue;
            };
            if let Ok(relative) = control_path.strip_prefix(&host_path) {
                translated.push(dest.join(relative));
            }
        }
    }
    (normalize(translated), None)
}

fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(current) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = current.file_name()?;
                missing.push(name.to_os_string());
                current = current.parent()?;
            }
            Err(_) => return None,
        }
    }
}

pub(crate) fn exposure(
    control_dirs: &[PathBuf],
    visible_roots: &[&Path],
) -> Option<ControlExposure> {
    control_dirs.iter().find_map(|control_dir| {
        visible_roots
            .iter()
            .find(|root| control_dir.starts_with(**root) || root.starts_with(control_dir))
            .copied()
            .map(|root| ControlExposure {
                control_dir: control_dir.clone(),
                visible_through: root.to_path_buf(),
            })
    })
}

fn normalize(mut dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.sort_by_key(|path| path.components().count());
    let mut normalized: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !normalized.iter().any(|parent| dir.starts_with(parent)) {
            normalized.push(dir);
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{ControlExposure, MaskPlan, exposure, normalize, plan};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    fn paths(values: &[&str]) -> Vec<PathBuf> {
        values.iter().map(|value| PathBuf::from(*value)).collect()
    }

    fn dir_set(values: &[&str]) -> HashSet<PathBuf> {
        paths(values).into_iter().collect()
    }

    fn is_dir(dirs: &HashSet<PathBuf>) -> impl Fn(&Path) -> bool + '_ {
        |path| dirs.contains(path)
    }

    #[test]
    fn plan_masks_a_control_dir_under_a_read_only_home() {
        let controls = paths(&["/home/u/.thegn-dev/run/thegn"]);
        let visible = [Path::new("/nix/store"), Path::new("/home/u")];
        let binds = [Path::new("/home/u/wt"), Path::new("/home/u")];
        let dirs = dir_set(&["/home/u/.thegn-dev/run/thegn"]);
        assert_eq!(
            plan(&controls, &visible, &binds, &is_dir(&dirs)),
            MaskPlan::Mask(paths(&["/home/u/.thegn-dev/run/thegn"]))
        );
    }

    #[test]
    fn plan_masks_the_deepest_existing_ancestor_when_the_dir_is_missing() {
        let controls = paths(&["/home/u/.run/thegn"]);
        let visible = [Path::new("/home/u")];
        let dirs = dir_set(&["/home/u/.run"]);
        assert_eq!(
            plan(&controls, &visible, &[], &is_dir(&dirs)),
            MaskPlan::Mask(paths(&["/home/u/.run"]))
        );
    }

    #[test]
    fn plan_dedupes_shared_mask_ancestors() {
        let controls = paths(&["/home/u/.run/first/thegn", "/home/u/.run/second/thegn"]);
        let visible = [Path::new("/home/u")];
        let dirs = dir_set(&["/home/u/.run"]);
        assert_eq!(
            plan(&controls, &visible, &[], &is_dir(&dirs)),
            MaskPlan::Mask(paths(&["/home/u/.run"]))
        );
    }

    #[test]
    fn plan_refuses_when_no_directory_below_the_bind_exists() {
        let controls = paths(&["/home/u/missing/run/thegn"]);
        let visible = [Path::new("/home/u")];
        let refusal = ControlExposure {
            control_dir: PathBuf::from("/home/u/missing/run/thegn"),
            visible_through: PathBuf::from("/home/u"),
        };
        assert_eq!(
            plan(&controls, &visible, &[], &|_| false),
            MaskPlan::Refuse(refusal)
        );
    }

    #[test]
    fn plan_refuses_when_the_mask_would_hide_a_bind() {
        let controls = paths(&["/home/u/proj"]);
        let visible = [Path::new("/home/u")];
        let binds = [Path::new("/home/u/proj/wt")];
        let dirs = dir_set(&["/home/u/proj"]);
        assert!(matches!(
            plan(&controls, &visible, &binds, &is_dir(&dirs)),
            MaskPlan::Refuse(_)
        ));
    }

    #[test]
    fn plan_leaves_the_default_run_user_layout_clear() {
        let controls = paths(&["/run/user/1000/thegn"]);
        let visible = [Path::new("/nix/store"), Path::new("/home/u")];
        let dirs = dir_set(&["/run/user/1000/thegn"]);
        assert_eq!(
            plan(&controls, &visible, &[], &is_dir(&dirs)),
            MaskPlan::Clear
        );
    }

    #[test]
    fn plan_matches_paths_by_component_not_prefix() {
        let controls = paths(&["/home/user2/run/thegn"]);
        let visible = [Path::new("/home/user")];
        let dirs = dir_set(&["/home/user2/run/thegn"]);
        assert_eq!(
            plan(&controls, &visible, &[], &is_dir(&dirs)),
            MaskPlan::Clear
        );
    }

    #[test]
    fn exposure_names_the_covering_mount() {
        assert_eq!(
            exposure(
                &paths(&["/srv/thegn"]),
                &[Path::new("/srv"), Path::new("/home")]
            ),
            Some(ControlExposure {
                control_dir: PathBuf::from("/srv/thegn"),
                visible_through: PathBuf::from("/srv"),
            })
        );
    }

    #[test]
    fn exposure_is_clear_when_no_mount_covers_a_control_dir() {
        assert_eq!(
            exposure(
                &paths(&["/srv/thegn"]),
                &[Path::new("/home"), Path::new("/run/user")]
            ),
            None
        );
    }

    #[test]
    fn planner_translates_a_control_dir_through_a_renamed_mount() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let control = root.path().join("run/thegn");
        std::fs::create_dir_all(&control).unwrap();
        let (translated, refusal) = in_sandbox_paths(
            std::slice::from_ref(&control),
            &[(root.path(), Path::new("/data"))],
        );
        assert_eq!(refusal, None);
        assert_eq!(translated, paths(&["/data/run/thegn"]));
        let visible = [Path::new("/data")];
        let dirs = dir_set(&["/data/run/thegn"]);
        assert_eq!(
            plan(&translated, &visible, &[], &is_dir(&dirs)),
            MaskPlan::Mask(paths(&["/data/run/thegn"]))
        );
    }

    #[test]
    fn planner_resolves_a_symlinked_control_path_before_mount_matching() {
        let temp = crate::testenv::symlinked_tempdir();
        let control = temp.path().join("run/thegn");
        std::fs::create_dir_all(&control).unwrap();
        let (translated, refusal) = in_sandbox_paths(
            std::slice::from_ref(&control),
            &[(temp.real_path(), Path::new("/data"))],
        );
        assert_eq!(refusal, None);
        assert_eq!(translated, paths(&["/data/run/thegn"]));
    }

    #[test]
    fn planner_keeps_a_missing_control_tail_beneath_a_canonical_mount() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let control = root.path().join("missing/run/thegn");
        let (translated, refusal) = in_sandbox_paths(
            std::slice::from_ref(&control),
            &[(root.path(), Path::new("/data"))],
        );
        assert_eq!(refusal, None);
        assert_eq!(translated, paths(&["/data/missing/run/thegn"]));
    }

    #[test]
    fn planner_refuses_an_ambiguous_lexical_match_to_an_uncanonicalized_mount() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let missing_mount = root.path().join("missing");
        let control = missing_mount.join("run/thegn");
        let (translated, refusal) = in_sandbox_paths(
            std::slice::from_ref(&control),
            &[(missing_mount.as_path(), Path::new("/data"))],
        );
        assert!(translated.is_empty());
        assert_eq!(
            refusal,
            Some(ControlExposure {
                control_dir: control,
                visible_through: PathBuf::from("/data"),
            })
        );
    }

    #[test]
    fn planner_leaves_a_canonical_control_outside_all_mount_hosts_unmapped() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let control = root.path().join("run/thegn");
        std::fs::create_dir_all(&control).unwrap();
        let other = tempfile::tempdir_in("/tmp").unwrap();
        let (translated, refusal) = in_sandbox_paths(
            std::slice::from_ref(&control),
            &[(other.path(), Path::new("/data"))],
        );
        assert!(translated.is_empty());
        assert_eq!(refusal, None);
    }

    #[test]
    fn exposure_reports_a_mount_inside_the_control_directory() {
        assert_eq!(
            exposure(
                &paths(&["/run/user/42/thegn"]),
                &[Path::new("/run/user/42/thegn/key")]
            ),
            Some(ControlExposure {
                control_dir: PathBuf::from("/run/user/42/thegn"),
                visible_through: PathBuf::from("/run/user/42/thegn/key"),
            })
        );
    }

    #[test]
    fn control_dirs_dedupes_nested_entries() {
        assert_eq!(
            normalize(paths(&[
                "/run/user/42/thegn",
                "/run/user/42",
                "/run/user/42"
            ])),
            paths(&["/run/user/42"])
        );
    }
}
