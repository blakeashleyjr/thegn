//! What the `standard` profile hides from a read-only `$HOME` (THE-2695).
//!
//! `standard` binds `$HOME` read-only so the user's shell, prompt and tools
//! work. Read-only is integrity, not confidentiality, so the resolver also
//! lists the host paths a standard pane must not read
//! ([`standard_hidden_paths`]): well-known credential locations, the private
//! entries of `~/.ssh`, and the git repositories that sit next to the worktree
//! or its main checkout. Each backend masks them after the bind that exposes
//! them: bwrap and local podman/docker with an empty tmpfs (directory) or a
//! read-only `/dev/null` bind (file), the systemd backend with
//! `InaccessiblePaths=`. A hidden path that holds an explicit sandbox mount is
//! left alone: the user, or a bundle identity, asked for it.
//!
//! `sealed` never mounts `$HOME` and does not use this list.

use std::path::{Path, PathBuf};

use crate::sandbox_runtime_escape::{EscapeKind, EscapeMask};

/// `$HOME`-relative credential locations hidden from a `standard` pane: cloud
/// CLI logins, token files, password stores, keyrings, browser profiles and
/// gpg private keys. `~/.ssh` is handled entry by entry, see
/// [`ssh_entry_is_public`]. The agent's own login (`~/.claude`, `~/.codex`)
/// stays readable: the pane's coding agent needs it.
pub const STANDARD_CREDENTIAL_PATHS: &[&str] = &[
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".config/gh",
    ".config/hub",
    ".config/glab-cli",
    ".config/doctl",
    ".config/hcloud",
    ".config/op",
    ".config/sops",
    ".config/age",
    ".config/rclone",
    ".secrets",
    ".password-store",
    ".vault-token",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".terraform.d/credentials.tfrc.json",
    ".gnupg/private-keys-v1.d",
    ".gnupg/secring.gpg",
    ".local/share/keyrings",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
];

/// Most directory entries read from one directory (`~/.ssh` or a repository
/// parent). A directory larger than this is only partly scanned.
const MAX_SCAN: usize = 4096;

/// Entries of `~/.ssh` that hold no secret and stay readable, so git over ssh
/// works through the ssh agent: host keys, the client config and public keys.
/// Every other entry (private keys, sockets, anything unrecognised) is hidden.
pub fn ssh_entry_is_public(name: &str) -> bool {
    matches!(
        name,
        "known_hosts" | "known_hosts.old" | "config" | "authorized_keys"
    ) || name.ends_with(".pub")
}

/// The canonical host path to hide for `path`, or `None` when it is missing or
/// must not be hidden: a Nix store path (world-readable and not a secret), or
/// `home` itself and its ancestors (that would be the `sealed` profile).
fn hide_target(path: &Path, home_canon: &Path) -> Option<PathBuf> {
    std::fs::symlink_metadata(path).ok()?;
    let canon = path.canonicalize().ok()?;
    if canon.starts_with("/nix/store") || home_canon.starts_with(&canon) {
        return None;
    }
    Some(canon)
}

/// Whether `dir` is a git repository or worktree: it has a `.git` entry, or it
/// is a bare repository (`HEAD` file plus `objects` directory).
fn is_repository(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok()
        || (dir.join("HEAD").is_file() && dir.join("objects").is_dir())
}

/// The host paths a `standard` pane must not read, canonical and de-duplicated:
///
/// - each [`STANDARD_CREDENTIAL_PATHS`] entry that exists under `home`;
/// - each non-public entry of `~/.ssh` ([`ssh_entry_is_public`]);
/// - each git repository (not dot-named) in the parent directory of the
///   worktree or of the main checkout, other than the worktree, its main
///   checkout and its git common dir.
///
/// A symlinked credential is hidden at its resolved target. Paths outside
/// `home` that no sandbox bind exposes produce no mask at launch.
pub fn standard_hidden_paths(
    home: &Path,
    worktree: &Path,
    git_common: Option<&Path>,
) -> Vec<String> {
    if !home.is_absolute() {
        return Vec::new();
    }
    let Ok(home_canon) = home.canonicalize() else {
        return Vec::new();
    };
    let mut hidden: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !hidden.contains(&path) {
            hidden.push(path);
        }
    };

    for rel in STANDARD_CREDENTIAL_PATHS {
        if let Some(target) = hide_target(&home.join(rel), &home_canon) {
            push(target);
        }
    }

    let ssh = home.join(".ssh");
    if let Ok(entries) = std::fs::read_dir(&ssh) {
        for entry in entries.flatten().take(MAX_SCAN) {
            let name = entry.file_name();
            if ssh_entry_is_public(&name.to_string_lossy()) {
                continue;
            }
            if let Some(target) = hide_target(&ssh.join(&name), &home_canon) {
                push(target);
            }
        }
    }

    let canon = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let worktree = canon(worktree);
    let common = git_common.map(canon);
    let repo_root = common.as_ref().map(|common| {
        if common.file_name().is_some_and(|name| name == ".git") {
            common
                .parent()
                .map_or_else(|| common.clone(), Path::to_path_buf)
        } else {
            common.clone()
        }
    });
    let mut keep: Vec<&Path> = vec![worktree.as_path()];
    keep.extend(common.as_deref());
    keep.extend(repo_root.as_deref());
    let mut parents: Vec<&Path> = Vec::new();
    for parent in [
        worktree.parent(),
        repo_root.as_deref().and_then(Path::parent),
    ]
    .into_iter()
    .flatten()
    {
        if parent.starts_with(&home_canon) && !parents.contains(&parent) {
            parents.push(parent);
        }
    }
    for parent in parents {
        let Ok(entries) = std::fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_SCAN) {
            // Dot-named entries are dotfiles (a git-managed `~/.config`, shell
            // plugin clones), never sibling projects; hiding them would break
            // the shell.
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() || !is_repository(&path) {
                continue;
            }
            let Some(target) = hide_target(&path, &home_canon) else {
                continue;
            };
            // The worktree, its main checkout and its common dir stay visible,
            // and so does any directory that contains one of them.
            if keep.iter().any(|kept| kept.starts_with(&target)) {
                continue;
            }
            push(target);
        }
    }

    hidden
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// The masks for `hidden` (canonical host paths) as seen through `mounts`
/// (`(host, dest)` pairs), reading each path's kind from the host and
/// canonicalizing each mount host, so a `$HOME` reached through a system link
/// (`/home -> /var/home`) still matches. A hidden path at or above an explicit
/// mount destination is not masked (see the module docs). Shared by the bwrap
/// argv and the local OCI argv.
pub(crate) fn host_hidden_masks(
    hidden: &[String],
    mounts: &[(&Path, &Path)],
    explicit_dests: &[&Path],
) -> Vec<EscapeMask> {
    if hidden.is_empty() {
        return Vec::new();
    }
    let paths: Vec<(PathBuf, EscapeKind)> = hidden
        .iter()
        .filter_map(|path| {
            let path = PathBuf::from(path);
            let kind = crate::sandbox_runtime_escape::kind_from_metadata(
                std::fs::symlink_metadata(&path).ok().as_ref(),
            )?;
            Some((path, kind))
        })
        .collect();
    let canonical_hosts: Vec<PathBuf> = mounts
        .iter()
        .map(|(host, _)| std::fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf()))
        .collect();
    let canonical_refs: Vec<(&Path, &Path)> = canonical_hosts
        .iter()
        .zip(mounts)
        .map(|(host, (_, dest))| (host.as_path(), *dest))
        .collect();
    crate::sandbox_runtime_escape::plan_escape_masks(&paths, &canonical_refs, explicit_dests)
}

/// The `systemd-run` properties hiding `hidden`: one `InaccessiblePaths=` per
/// path, skipping a path that contains the worktree or a mount destination
/// (making it inaccessible would hide them) or that systemd cannot parse.
pub(crate) fn systemd_hidden_props(
    hidden: &[String],
    worktree: &Path,
    mount_dests: &[&Path],
) -> Vec<String> {
    hidden
        .iter()
        .filter(|path| crate::sandbox_mounts::systemd_path_refusal(path).is_none())
        .filter(|path| {
            let path = Path::new(path.as_str());
            !worktree.starts_with(path) && !mount_dests.iter().any(|dest| dest.starts_with(path))
        })
        .map(|path| format!("InaccessiblePaths=-{path}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        STANDARD_CREDENTIAL_PATHS, host_hidden_masks, ssh_entry_is_public, standard_hidden_paths,
        systemd_hidden_props,
    };
    use crate::sandbox_runtime_escape::EscapeMask;
    use std::path::{Path, PathBuf};

    struct Fixture {
        _temp: tempfile::TempDir,
        home: PathBuf,
        worktree: PathBuf,
        common: PathBuf,
    }

    /// A home with credentials, an `~/.ssh`, a main checkout `code/app` with a
    /// linked worktree under `wt/app/lane`, and sibling repositories in both
    /// parents.
    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap().join("home");
        for dir in [
            ".aws",
            ".config/gcloud",
            ".config/gh",
            ".ssh",
            ".claude/todos",
            ".config/nvim/.git",
            "code/app/.git",
            "code/other/.git",
            "code/notes",
            "code/bare.git/objects",
            "wt/app/lane",
            "wt/app/other-lane",
        ] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        for file in [
            ".netrc",
            ".docker/config.json",
            ".ssh/id_ed25519",
            ".ssh/id_ed25519.pub",
            ".ssh/known_hosts",
            ".ssh/config",
            "code/bare.git/HEAD",
            "wt/app/lane/.git",
            "wt/app/other-lane/.git",
        ] {
            let path = home.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        Fixture {
            worktree: home.join("wt/app/lane"),
            common: home.join("code/app/.git"),
            home,
            _temp: temp,
        }
    }

    fn hidden(f: &Fixture) -> Vec<String> {
        standard_hidden_paths(&f.home, &f.worktree, Some(f.common.as_path()))
    }

    fn s(path: PathBuf) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn credential_locations_are_hidden() {
        let f = fixture();
        let h = hidden(&f);
        for rel in [
            ".aws",
            ".config/gcloud",
            ".config/gh",
            ".netrc",
            ".docker/config.json",
        ] {
            assert!(h.contains(&s(f.home.join(rel))), "{rel} missing from {h:?}");
        }
        // Absent entries add nothing; the agent's own login stays readable.
        assert!(!h.contains(&s(f.home.join(".kube"))));
        assert!(!h.iter().any(|p| p.contains("/.claude")), "{h:?}");
        assert!(!STANDARD_CREDENTIAL_PATHS.contains(&".claude"));
    }

    #[test]
    fn ssh_private_entries_are_hidden_and_public_ones_stay() {
        let f = fixture();
        let h = hidden(&f);
        assert!(h.contains(&s(f.home.join(".ssh/id_ed25519"))), "{h:?}");
        for public in ["id_ed25519.pub", "known_hosts", "config"] {
            assert!(
                !h.contains(&s(f.home.join(".ssh").join(public))),
                "{public}"
            );
        }
        assert!(!h.contains(&s(f.home.join(".ssh"))));
        assert!(ssh_entry_is_public("known_hosts.old"));
        assert!(ssh_entry_is_public("work.pub"));
        assert!(!ssh_entry_is_public("id_rsa"));
        assert!(!ssh_entry_is_public("agent.sock"));
    }

    #[test]
    fn sibling_repositories_are_hidden_but_the_worktree_and_checkout_stay() {
        let f = fixture();
        let h = hidden(&f);
        assert!(h.contains(&s(f.home.join("code/other"))), "{h:?}");
        assert!(h.contains(&s(f.home.join("code/bare.git"))), "{h:?}");
        assert!(h.contains(&s(f.home.join("wt/app/other-lane"))), "{h:?}");
        for kept in ["code/app", "wt/app/lane", "code/notes", ".config/nvim"] {
            assert!(!h.contains(&s(f.home.join(kept))), "{kept} hidden: {h:?}");
        }
    }

    #[test]
    fn a_worktree_directly_in_home_hides_only_non_dot_sibling_repos() {
        let f = fixture();
        std::fs::create_dir_all(f.home.join("proj/.git")).unwrap();
        std::fs::create_dir_all(f.home.join("neighbour/.git")).unwrap();
        let proj = f.home.join("proj");
        let proj_git = proj.join(".git");
        let h = standard_hidden_paths(&f.home, &proj, Some(proj_git.as_path()));
        assert!(h.contains(&s(f.home.join("neighbour"))), "{h:?}");
        assert!(!h.contains(&s(f.home.join("proj"))));
        assert!(!h.contains(&s(f.home.join(".config/nvim"))));
        assert!(!h.contains(&s(f.home.clone())));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_credential_is_hidden_at_its_target_and_home_never() {
        let f = fixture();
        let elsewhere = f.home.join("vault/aws");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::remove_dir_all(f.home.join(".aws")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.home.join(".aws")).unwrap();
        // A credential link to home itself must not hide home.
        std::os::unix::fs::symlink(&f.home, f.home.join(".azure")).unwrap();
        let h = hidden(&f);
        assert!(h.contains(&s(elsewhere)), "{h:?}");
        assert!(!h.contains(&s(f.home.clone())), "{h:?}");
    }

    #[test]
    fn a_relative_home_hides_nothing() {
        assert!(standard_hidden_paths(Path::new("home"), Path::new("/wt"), None).is_empty());
    }

    #[test]
    fn masks_translate_through_the_home_bind_and_honour_explicit_mounts() {
        let f = fixture();
        let h = hidden(&f);
        let home = f.home.as_path();
        let gh = f.home.join(".config/gh");
        let wt = f.worktree.as_path();
        let masks = host_hidden_masks(&h, &[(home, home)], &[home, wt]);
        assert!(
            masks.contains(&EscapeMask::Tmpfs(f.home.join(".aws"))),
            "{masks:?}"
        );
        assert!(masks.contains(&EscapeMask::NullBind(f.home.join(".netrc"))));
        assert!(masks.contains(&EscapeMask::NullBind(f.home.join(".ssh/id_ed25519"))));
        assert!(masks.contains(&EscapeMask::Tmpfs(gh.clone())));
        assert!(masks.contains(&EscapeMask::Tmpfs(f.home.join("code/other"))));
        // An explicit mount of `~/.config/gh` (a bundle identity) keeps it.
        let honoured = host_hidden_masks(&h, &[(home, home)], &[home, wt, gh.as_path()]);
        assert!(!honoured.contains(&EscapeMask::Tmpfs(gh)), "{honoured:?}");
        // Nothing binds the home: nothing to mask.
        let other = Path::new("/nonexistent-bind");
        assert!(host_hidden_masks(&h, &[(other, other)], &[]).is_empty());
        assert!(host_hidden_masks(&[], &[(home, home)], &[]).is_empty());
    }

    #[test]
    fn systemd_props_skip_paths_holding_the_worktree_or_a_mount() {
        let hidden = vec![
            "/home/u/.aws".to_string(),
            "/home/u/code/other".to_string(),
            "/home/u/code".to_string(),
            "/home/u/bad path".to_string(),
        ];
        let props = systemd_hidden_props(
            &hidden,
            Path::new("/home/u/code/app"),
            &[Path::new("/home/u/code/other/lib")],
        );
        assert_eq!(props, vec!["InaccessiblePaths=-/home/u/.aws".to_string()]);
    }
}
