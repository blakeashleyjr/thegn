//! Host-escape entries under `$XDG_RUNTIME_DIR` (THE-2009).
//!
//! A sandbox mount that exposes the user runtime directory (for example
//! `/run/user`) also exposes the user bus, the podman/docker sockets and the
//! systemd user manager: any of them runs code outside the sandbox
//! (`systemd-run --user`, `podman run`). bwrap masks these entries after every
//! bind; the systemd backend makes them inaccessible. This module is pure: the
//! filesystem probe is injected, and `config validate` passes the runtime dir
//! and home from its call site.

use std::path::{Path, PathBuf};

/// What a runtime-dir entry is on the host, which decides how bwrap masks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscapeKind {
    /// A socket (or any other non-directory): masked with a read-only
    /// `/dev/null` bind, since a tmpfs cannot cover a non-directory.
    Socket,
    /// A directory holding sockets: masked with an empty tmpfs.
    Dir,
}

/// One bwrap mask, already translated into the sandbox's path space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EscapeMask {
    Tmpfs(PathBuf),
    NullBind(PathBuf),
}

/// Runtime-dir entries that grant host command authority, relative to the
/// runtime dir: the user bus, the docker socket, and the systemd/podman/docker
/// socket directories.
pub(crate) const ESCAPE_ENTRIES: &[(&str, EscapeKind)] = &[
    ("bus", EscapeKind::Socket),
    ("docker.sock", EscapeKind::Socket),
    ("systemd", EscapeKind::Dir),
    ("podman", EscapeKind::Dir),
    ("docker", EscapeKind::Dir),
];

/// The escape entries present on the host. `kind_of` reports an entry's kind
/// from `symlink_metadata` (never following a symlink), or `None` when it is
/// missing. The host kind wins over the table's expectation.
pub(crate) fn host_escape_paths(
    runtime_dir: &Path,
    kind_of: &dyn Fn(&Path) -> Option<EscapeKind>,
) -> Vec<(PathBuf, EscapeKind)> {
    ESCAPE_ENTRIES
        .iter()
        .filter_map(|(name, _)| {
            let path = runtime_dir.join(name);
            kind_of(&path).map(|kind| (path, kind))
        })
        .collect()
}

/// Map a `symlink_metadata` result onto an [`EscapeKind`]. A symlink is
/// reported as a non-directory and so masked with a null bind.
pub(crate) fn kind_from_metadata(metadata: Option<&std::fs::Metadata>) -> Option<EscapeKind> {
    metadata.map(|metadata| {
        if metadata.file_type().is_dir() {
            EscapeKind::Dir
        } else {
            EscapeKind::Socket
        }
    })
}

/// The bwrap masks for `escapes` as seen through `mounts` (`(host, dest)`
/// pairs). An escape is translated through every mount whose host path is a
/// component-wise prefix of it. An escape is skipped when an explicit user
/// mount destination equals it or lies inside it: the user asked for that
/// path, and a mask would hide the bind. An explicit mount of an ancestor
/// (the runtime dir itself, `/run/user`) is exactly the exposure, so it never
/// suppresses a mask.
pub(crate) fn plan_escape_masks(
    escapes: &[(PathBuf, EscapeKind)],
    mounts: &[(&Path, &Path)],
    explicit_dests: &[&Path],
) -> Vec<EscapeMask> {
    let mut masks = Vec::new();
    for (escape, kind) in escapes {
        for (host, dest) in mounts {
            let Ok(relative) = escape.strip_prefix(host) else {
                continue;
            };
            let translated = if relative.as_os_str().is_empty() {
                dest.to_path_buf()
            } else {
                dest.join(relative)
            };
            // A tmpfs here would also hide any explicit bind at or beneath it.
            let honoured = explicit_dests
                .iter()
                .any(|explicit| explicit.starts_with(&translated));
            if honoured {
                continue;
            }
            let mask = match kind {
                EscapeKind::Dir => EscapeMask::Tmpfs(translated),
                EscapeKind::Socket => EscapeMask::NullBind(translated),
            };
            if !masks.contains(&mask) {
                masks.push(mask);
            }
        }
    }
    masks
}

/// Whether any mount host path equals or is an ancestor of `runtime_dir`.
pub(crate) fn mounts_expose(runtime_dir: &Path, mounts: &[(&Path, &Path)]) -> bool {
    mounts.iter().any(|(host, _)| runtime_dir.starts_with(host))
}

/// `config validate` warnings for mounts that expose `$XDG_RUNTIME_DIR` or make
/// `~/.claude-profiles` writable. `mounts` pairs a config path label with the raw
/// spec. Unparseable specs are skipped (the parser warning already covers them).
/// With no runtime dir, any `/run/user/<n>` covered by the mount counts.
pub fn exposure_warnings(
    mounts: &[(String, String)],
    runtime_dir: Option<&Path>,
    home: &Path,
) -> Vec<String> {
    let profiles = home.join(".claude-profiles");
    let mut warnings = Vec::new();
    for (label, spec) in mounts {
        let Ok(mount) = crate::sandbox_mounts::parse_mount_checked(spec) else {
            continue;
        };
        let host = Path::new(&mount.host);
        let exposes_runtime = match runtime_dir {
            Some(runtime_dir) => runtime_dir.starts_with(host),
            None => covers_a_uid_runtime_dir(host),
        };
        if exposes_runtime {
            warnings.push(format!(
                "`{label}` `{spec}` exposes $XDG_RUNTIME_DIR (user bus, podman and systemd sockets): sandbox escape on container/remote backends; bwrap masks bus/podman/systemd/docker. Mount only what a pane needs, e.g. `/run/user/<uid>/gnupg` or the ssh-agent socket."
            ));
        }
        if !mount.ro && profiles.starts_with(host) {
            warnings.push(format!(
                "`{label}` `{spec}`: a writable mount of ~/.claude-profiles lets a worker rewrite every profile's settings and hooks; mount only the one profile directory"
            ));
        }
    }
    warnings
}

/// `host` is `/run/user`, `/run`, `/` or a `/run/user/<n>` directory itself.
fn covers_a_uid_runtime_dir(host: &Path) -> bool {
    let run_user = Path::new("/run/user");
    if run_user.starts_with(host) {
        return true;
    }
    host.strip_prefix(run_user).is_ok_and(|relative| {
        let mut components = relative.components();
        matches!(
            (components.next(), components.next()),
            (Some(std::path::Component::Normal(uid)), None)
                if uid.to_str().is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()))
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ESCAPE_ENTRIES, EscapeKind, EscapeMask, exposure_warnings, host_escape_paths,
        kind_from_metadata, mounts_expose, plan_escape_masks,
    };
    use std::path::{Path, PathBuf};

    fn rt() -> PathBuf {
        PathBuf::from("/run/user/1000")
    }

    fn all_present(path: &Path) -> Option<EscapeKind> {
        ESCAPE_ENTRIES
            .iter()
            .find(|(name, _)| path.file_name().is_some_and(|file| file == *name))
            .map(|(_, kind)| *kind)
    }

    #[test]
    fn host_escape_paths_lists_only_present_entries() {
        let present = host_escape_paths(&rt(), &|path| {
            if path.ends_with("bus") {
                Some(EscapeKind::Socket)
            } else if path.ends_with("podman") {
                Some(EscapeKind::Dir)
            } else {
                None
            }
        });
        assert_eq!(
            present,
            vec![
                (rt().join("bus"), EscapeKind::Socket),
                (rt().join("podman"), EscapeKind::Dir),
            ]
        );
        assert!(host_escape_paths(&rt(), &|_| None).is_empty());
        assert_eq!(host_escape_paths(&rt(), &all_present).len(), 5);
    }

    #[test]
    fn kind_from_metadata_maps_dirs_files_and_missing() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("bus");
        std::fs::write(&file, b"").unwrap();
        let dir_meta = std::fs::symlink_metadata(temp.path()).unwrap();
        let file_meta = std::fs::symlink_metadata(&file).unwrap();
        assert_eq!(kind_from_metadata(Some(&dir_meta)), Some(EscapeKind::Dir));
        assert_eq!(
            kind_from_metadata(Some(&file_meta)),
            Some(EscapeKind::Socket)
        );
        assert_eq!(kind_from_metadata(None), None);
    }

    #[test]
    fn dirs_get_tmpfs_and_sockets_get_null_binds() {
        let escapes = host_escape_paths(&rt(), &all_present);
        let host = Path::new("/run/user");
        let masks = plan_escape_masks(&escapes, &[(host, host)], &[host]);
        assert_eq!(
            masks,
            vec![
                EscapeMask::NullBind(rt().join("bus")),
                EscapeMask::NullBind(rt().join("docker.sock")),
                EscapeMask::Tmpfs(rt().join("systemd")),
                EscapeMask::Tmpfs(rt().join("podman")),
                EscapeMask::Tmpfs(rt().join("docker")),
            ]
        );
    }

    #[test]
    fn escapes_translate_through_a_renamed_mount() {
        let escapes = vec![(rt().join("podman"), EscapeKind::Dir)];
        let host = Path::new("/run/user");
        let dest = Path::new("/r");
        assert_eq!(
            plan_escape_masks(&escapes, &[(host, dest)], &[dest]),
            vec![EscapeMask::Tmpfs(PathBuf::from("/r/1000/podman"))]
        );
        let exact = rt();
        assert_eq!(
            plan_escape_masks(&escapes, &[(exact.as_path(), dest)], &[dest]),
            vec![EscapeMask::Tmpfs(PathBuf::from("/r/podman"))]
        );
    }

    #[test]
    fn a_mount_of_the_escape_itself_is_honoured() {
        let podman = rt().join("podman");
        let escapes = vec![(podman.clone(), EscapeKind::Dir)];
        assert!(
            plan_escape_masks(
                &escapes,
                &[(podman.as_path(), podman.as_path())],
                &[podman.as_path()]
            )
            .is_empty()
        );
    }

    #[test]
    fn an_explicit_podman_socket_mount_suppresses_only_the_podman_mask() {
        let escapes = host_escape_paths(&rt(), &all_present);
        let run_user = Path::new("/run/user");
        let socket = rt().join("podman/podman.sock");
        let masks = plan_escape_masks(
            &escapes,
            &[(run_user, run_user), (socket.as_path(), socket.as_path())],
            &[run_user, socket.as_path()],
        );
        assert!(!masks.contains(&EscapeMask::Tmpfs(rt().join("podman"))));
        assert!(masks.contains(&EscapeMask::NullBind(rt().join("bus"))));
        assert!(masks.contains(&EscapeMask::Tmpfs(rt().join("systemd"))));
        assert_eq!(masks.len(), 4);
    }

    #[test]
    fn an_explicit_runtime_dir_mount_does_not_suppress_masks() {
        // Mounting the runtime dir itself is the exposure being masked.
        let escapes = vec![(rt().join("bus"), EscapeKind::Socket)];
        let run_user = Path::new("/run/user");
        let exact = rt();
        let masks = plan_escape_masks(
            &escapes,
            &[(run_user, run_user), (exact.as_path(), exact.as_path())],
            &[run_user, exact.as_path()],
        );
        assert_eq!(masks, vec![EscapeMask::NullBind(rt().join("bus"))]);
    }

    #[test]
    fn prefixes_are_component_wise() {
        let escapes = vec![(rt().join("bus"), EscapeKind::Socket)];
        let near = Path::new("/run/user/10");
        assert!(plan_escape_masks(&escapes, &[(near, near)], &[near]).is_empty());
        assert!(!mounts_expose(&rt(), &[(near, near)]));
        let run_user = Path::new("/run/user");
        assert!(mounts_expose(&rt(), &[(run_user, run_user)]));
    }

    #[test]
    fn duplicate_masks_are_emitted_once() {
        let escapes = vec![(rt().join("bus"), EscapeKind::Socket)];
        let run_user = Path::new("/run/user");
        assert_eq!(
            plan_escape_masks(&escapes, &[(run_user, run_user), (run_user, run_user)], &[]),
            vec![EscapeMask::NullBind(rt().join("bus"))]
        );
    }

    fn warn(spec: &str, runtime_dir: Option<&Path>) -> Vec<String> {
        exposure_warnings(
            &[("sandbox.mounts[0]".to_string(), spec.to_string())],
            runtime_dir,
            Path::new("/h"),
        )
    }

    #[test]
    fn runtime_dir_exposure_is_warned() {
        let runtime = rt();
        for spec in ["/run/user", "/run/user/1000", "/run/user:ro", "/run"] {
            let warnings = warn(spec, Some(&runtime));
            assert_eq!(warnings.len(), 1, "{spec}: {warnings:?}");
            assert!(warnings[0].contains("exposes $XDG_RUNTIME_DIR"));
            assert!(warnings[0].contains("sandbox.mounts[0]"));
            assert!(warnings[0].contains(spec));
        }
        assert!(warn("/run/user/1000/gnupg", Some(&runtime)).is_empty());
        assert!(warn("/run/user/10", Some(&runtime)).is_empty());
        assert!(warn("not-absolute", Some(&runtime)).is_empty());
    }

    #[test]
    fn unknown_runtime_dir_falls_back_to_run_user_uid() {
        assert_eq!(warn("/run/user", None).len(), 1);
        assert_eq!(warn("/run/user/1000", None).len(), 1);
        assert_eq!(warn("/", None).len(), 1);
        assert!(warn("/run/user/1000/gnupg", None).is_empty());
        assert!(warn("/run/user/abc", None).is_empty());
        assert!(warn("/srv", None).is_empty());
    }

    #[test]
    fn writable_claude_profiles_mount_is_warned() {
        let warnings = warn("/h/.claude-profiles:rw", None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("~/.claude-profiles"));
        assert_eq!(warn("/h/.claude-profiles", None).len(), 1);
        assert_eq!(warn("/h:rw", None).len(), 1);
        assert!(warn("/h/.claude-profiles/x:rw", None).is_empty());
        assert!(warn("/h/.claude-profiles:ro", None).is_empty());
    }

    #[test]
    fn tilde_claude_profiles_mount_expands_against_home() {
        let home = crate::util::home();
        let warnings = exposure_warnings(
            &[(
                "sandbox.mounts[0]".to_string(),
                "~/.claude-profiles:rw".to_string(),
            )],
            Some(Path::new("/nonexistent-runtime")),
            &home,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
