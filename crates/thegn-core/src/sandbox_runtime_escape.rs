//! Host-escape entries under `$XDG_RUNTIME_DIR` (THE-2009).
//!
//! A sandbox mount that exposes the user runtime directory (for example
//! `/run/user`) also exposes the user bus, the podman/docker sockets and the
//! systemd user manager: any of them runs code outside the sandbox
//! (`systemd-run --user`, `podman run`). bwrap and local podman/docker mask
//! these entries after every bind; the systemd backend makes them
//! inaccessible. The planner is pure (the filesystem probe is injected);
//! [`host_runtime_escape_masks`] is the one wrapper that reads the host, and
//! `config validate` passes the runtime dir and home from its call site.

use std::path::{Path, PathBuf};

/// Container-engine sockets that a devcontainer bind must not expose by default.
pub fn engine_socket_paths(home: Option<&Path>, runtime_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(home.join(".docker/run/docker.sock"));
    }
    paths.extend([
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from("/run/docker.sock"),
        PathBuf::from("/run/podman/podman.sock"),
        PathBuf::from("/run/containerd/containerd.sock"),
    ]);
    if let Some(runtime_dir) = runtime_dir {
        paths.push(runtime_dir.join("podman/podman.sock"));
    }
    paths
}

/// Thegn state and runtime directories protected from devcontainer binds.
pub(crate) fn thegn_control_paths(state_home: &Path, runtime_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = vec![state_home.join("thegn")];
    if let Some(runtime_dir) = runtime_dir {
        paths.push(runtime_dir.join("thegn"));
    }
    paths
}

/// Canonicalize an existing path, or resolve its deepest existing ancestor and
/// append the remaining components. This follows symlinks even when the leaf
/// socket or directory has not been created yet.
pub(crate) fn canonical_or_normalized(path: &Path) -> PathBuf {
    canonical_or_normalized_inner(path, 0)
}

fn canonical_or_normalized_inner(path: &Path, depth: usize) -> PathBuf {
    if depth < 40
        && let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
        && let Ok(target) = std::fs::read_link(path)
    {
        let target = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or_else(|| Path::new("/")).join(target)
        };
        return canonical_or_normalized_inner(&target, depth + 1);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    if let Ok(path) = std::fs::canonicalize(&absolute) {
        return path;
    }
    let mut missing = Vec::new();
    let mut ancestor = absolute.as_path();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
    }
    let mut resolved =
        std::fs::canonicalize(ancestor).unwrap_or_else(|_| lexical_normalize(&absolute));
    if resolved != lexical_normalize(&absolute) {
        for component in missing.iter().rev() {
            resolved.push(component);
        }
    }
    lexical_normalize(&resolved)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Whether a canonicalized source equals, or contains, one of the protected paths.
pub(crate) fn reaches_protected_path(source: &Path, protected: &[PathBuf]) -> bool {
    protected.iter().any(|path| {
        let path = canonical_or_normalized(path);
        source == path || path.starts_with(source)
    })
}

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

/// The masks a sandbox needs when its binds are `mounts` (`(host, dest)`
/// pairs, host paths already canonical) and the host runtime dir is
/// `runtime_dir` (canonical). Empty when no mount exposes the runtime dir.
/// Shared by the bwrap argv and the local OCI argv.
pub(crate) fn runtime_escape_masks(
    runtime_dir: &Path,
    mounts: &[(&Path, &Path)],
    explicit_dests: &[&Path],
    kind_of: &dyn Fn(&Path) -> Option<EscapeKind>,
) -> Vec<EscapeMask> {
    if !mounts_expose(runtime_dir, mounts) {
        return Vec::new();
    }
    let escapes = host_escape_paths(runtime_dir, kind_of);
    plan_escape_masks(&escapes, mounts, explicit_dests)
}

/// [`runtime_escape_masks`] against the real host: canonicalizes the runtime
/// dir and every mount host (a symlinked or `..` source such as `/var/run`
/// exposes the runtime dir too) and reads entry kinds with
/// `symlink_metadata`. No runtime dir, or the bare `/run/user` fallback,
/// yields no masks.
pub(crate) fn host_runtime_escape_masks(
    raw_runtime_dir: Option<&Path>,
    mounts: &[(&Path, &Path)],
    explicit_dests: &[&Path],
) -> Vec<EscapeMask> {
    let Some(raw_runtime_dir) = raw_runtime_dir.filter(|dir| *dir != Path::new("/run/user")) else {
        return Vec::new();
    };
    let runtime_dir =
        std::fs::canonicalize(raw_runtime_dir).unwrap_or_else(|_| raw_runtime_dir.to_path_buf());
    let canonical_hosts: Vec<PathBuf> = mounts
        .iter()
        .map(|(host, _)| std::fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf()))
        .collect();
    let canonical_refs: Vec<(&Path, &Path)> = canonical_hosts
        .iter()
        .zip(mounts)
        .map(|(host, (_, dest))| (host.as_path(), *dest))
        .collect();
    runtime_escape_masks(&runtime_dir, &canonical_refs, explicit_dests, &|path| {
        kind_from_metadata(std::fs::symlink_metadata(path).ok().as_ref())
    })
}

/// The podman/docker spelling of `masks`: a tmpfs per directory and a
/// read-only `/dev/null` bind per socket.
pub(crate) fn oci_mask_args(masks: &[EscapeMask]) -> Vec<String> {
    let mut args = Vec::new();
    for mask in masks {
        match mask {
            EscapeMask::Tmpfs(dest) => {
                args.extend(["--tmpfs".to_string(), dest.display().to_string()]);
            }
            EscapeMask::NullBind(dest) => {
                args.extend(["-v".to_string(), format!("/dev/null:{}:ro", dest.display())]);
            }
        }
    }
    args
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
        host_runtime_escape_masks, kind_from_metadata, mounts_expose, oci_mask_args,
        plan_escape_masks, runtime_escape_masks,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn engine_socket_paths_preserve_compose_guard_order() {
        assert_eq!(
            super::engine_socket_paths(
                Some(Path::new("/home/u")),
                Some(Path::new("/run/user/1000"))
            ),
            vec![
                PathBuf::from("/home/u/.docker/run/docker.sock"),
                PathBuf::from("/var/run/docker.sock"),
                PathBuf::from("/run/docker.sock"),
                PathBuf::from("/run/podman/podman.sock"),
                PathBuf::from("/run/containerd/containerd.sock"),
                PathBuf::from("/run/user/1000/podman/podman.sock"),
            ]
        );
    }

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

    #[test]
    fn runtime_escape_masks_needs_an_exposing_mount() {
        let run_user = Path::new("/run/user");
        let masks = runtime_escape_masks(&rt(), &[(run_user, run_user)], &[run_user], &all_present);
        assert_eq!(masks.len(), 5);
        assert_eq!(
            masks,
            plan_escape_masks(
                &host_escape_paths(&rt(), &all_present),
                &[(run_user, run_user)],
                &[run_user]
            )
        );
        let gnupg = rt().join("gnupg");
        assert!(
            runtime_escape_masks(
                &rt(),
                &[(gnupg.as_path(), gnupg.as_path())],
                &[],
                &all_present
            )
            .is_empty()
        );
        assert!(runtime_escape_masks(&rt(), &[], &[], &all_present).is_empty());
        // An ancestor mount listed as explicit never suppresses a mask.
        let masks = runtime_escape_masks(
            &rt(),
            &[(run_user, run_user)],
            &[run_user, Path::new("/run"), Path::new("/")],
            &all_present,
        );
        assert_eq!(masks.len(), 5);
    }

    #[test]
    fn host_runtime_escape_masks_probes_real_entries() {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let runtime = base.join("rt");
        std::fs::create_dir_all(runtime.join("systemd")).unwrap();
        std::fs::write(runtime.join("bus"), b"").unwrap();
        let masks = host_runtime_escape_masks(Some(&runtime), &[(&base, &base)], &[&base]);
        assert_eq!(
            masks,
            vec![
                EscapeMask::NullBind(runtime.join("bus")),
                EscapeMask::Tmpfs(runtime.join("systemd")),
            ]
        );
        // A non-canonical mount source still counts.
        let dotted = base.join("rt/..");
        assert_eq!(
            host_runtime_escape_masks(Some(&runtime), &[(&dotted, &base)], &[]).len(),
            2
        );
        assert!(host_runtime_escape_masks(None, &[(&base, &base)], &[]).is_empty());
        let fallback = Path::new("/run/user");
        assert!(host_runtime_escape_masks(Some(fallback), &[(fallback, fallback)], &[]).is_empty());
    }

    #[test]
    fn oci_mask_args_spell_tmpfs_and_null_binds() {
        assert_eq!(
            oci_mask_args(&[
                EscapeMask::Tmpfs(rt().join("systemd")),
                EscapeMask::NullBind(rt().join("bus")),
            ]),
            [
                "--tmpfs",
                "/run/user/1000/systemd",
                "-v",
                "/dev/null:/run/user/1000/bus:ro",
            ]
        );
        assert!(oci_mask_args(&[]).is_empty());
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
        assert_eq!(warn("/:ro", None).len(), 1);
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
