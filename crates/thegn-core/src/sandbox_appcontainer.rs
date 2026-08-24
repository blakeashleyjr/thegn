//! Planning for the Windows **AppContainer** backend: the container identity, the
//! filesystem grants a pane needs, and the capability SIDs its network policy
//! maps to.
//!
//! An AppContainer is the native peer of `bwrap`: the process runs under a token
//! carrying its own container SID, denied the filesystem, registry and object
//! namespace by default, and reaching the network only through capability SIDs.
//! There is no VM, no image, and — because it is the same filesystem seen
//! through a weaker token — **no path translation**. That is what makes it worth
//! having next to the OCI backends, which need both.
//!
//! # Why a plan, and not just a spawn
//!
//! Deny-by-default cuts both ways: a pane that cannot read `git.exe` is not a
//! sandboxed pane, it is a broken one. `C:\Program Files\Git` carries no
//! `ALL APPLICATION PACKAGES` ACE, so it is unreachable from a container until
//! something grants it — while `System32` does carry one, which is why `cmd.exe`
//! works out of the box.
//!
//! So selection has to know, *before* spawning, which paths need granting and
//! which of those it actually managed to grant. [`plan`] answers the first half
//! and is pure; the caller applies the grants and reports what it could not do.
//! thegn never elevates to force one through — `doctor` names the directory and
//! the exact `icacls` command instead, and a pane whose toolchain is unreachable
//! degrades to `host` rather than starting broken.
//!
//! # What this module does NOT do
//!
//! No Win32 calls live here. Deriving the SID, creating/deleting the profile and
//! spawning through the trampoline are the host's job (`thegn appcontainer-exec`);
//! keeping the decisions here means the whole table is unit-tested from the Linux
//! coverage gate, exactly like [`crate::sandbox_gitshim`].

use std::path::{Path, PathBuf};

use crate::config::Network;

/// Longest AppContainer profile name Windows accepts.
///
/// `CreateAppContainerProfile` documents a 64-character ceiling; a longer name
/// fails the call rather than being truncated for us.
pub const MAX_PROFILE_NAME: usize = 64;

/// One directory the container SID needs access to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub path: PathBuf,
    /// Read+execute is enough for a toolchain; the worktree needs writing.
    pub write: bool,
    /// What breaks without it — printed by `doctor` when the grant fails, so the
    /// user is told the consequence rather than just the path.
    pub needed_for: &'static str,
}

impl Grant {
    /// The `icacls` permission set for this grant.
    ///
    /// `(OI)(CI)` makes it inheritable by files and subdirectories, which is what
    /// a toolchain directory or worktree needs; without them only the directory
    /// entry itself is reachable.
    pub fn icacls_perms(&self) -> &'static str {
        if self.write {
            "(OI)(CI)(M)"
        } else {
            "(OI)(CI)(RX)"
        }
    }
}

/// Everything needed to run a pane in an AppContainer, decided up front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppContainerPlan {
    /// Profile name — deterministic per worktree, so create and teardown agree.
    pub profile: String,
    pub grants: Vec<Grant>,
    /// Well-known capability names granted to the token. Empty means no network.
    pub capabilities: Vec<&'static str>,
}

/// The deterministic profile name for a worktree.
///
/// Reuses [`crate::sandbox::container_name`] so an AppContainer and an OCI
/// container for the same worktree carry the same identity, then fits it to
/// Windows' 64-character ceiling by replacing the tail with a hash of the full
/// path. Truncating alone would collide two deep worktrees that share a prefix —
/// which is the common case, since they are siblings under one repo.
pub fn profile_name(worktree: &str) -> String {
    let base = crate::sandbox::container_name(worktree);
    if base.len() <= MAX_PROFILE_NAME {
        return base;
    }
    // `slugify` emits ASCII only, so byte slicing cannot split a character.
    let hash = crate::util::short_hash(worktree, 8);
    let head = &base[..MAX_PROFILE_NAME - hash.len() - 1];
    format!("{head}-{hash}")
}

/// The capability SIDs a network policy maps to.
///
/// An AppContainer has no network at all unless a capability grants it, which is
/// a stronger default than any OCI backend gives us — `Network::None` needs no
/// flag, it is simply the absence of one.
///
/// `privateNetworkClientServer` is deliberately NOT granted for `Nat`: it opens
/// the LAN, and a worktree pane wanting the internet does not imply wanting the
/// user's local network. `Host` is the escape hatch that asks for both.
pub fn capabilities_for(network: Network) -> Vec<&'static str> {
    match network {
        Network::None => Vec::new(),
        Network::Nat => vec!["internetClient"],
        Network::Host => vec![
            "internetClient",
            "internetClientServer",
            "privateNetworkClientServer",
        ],
    }
}

/// Decide the plan for a worktree.
///
/// `tools` are already-resolved absolute paths to programs the pane needs (the
/// shell, `git`, …). Passing them in rather than resolving here keeps this pure:
/// the caller does the `PATH` lookup, this decides what to do about it.
///
/// A tool already reachable by every container — anything under `System32` — is
/// skipped: granting it would be a no-op ACE on a system directory, which is a
/// change to the machine thegn has no business making.
pub fn plan(worktree: &Path, network: Network, tools: &[PathBuf]) -> AppContainerPlan {
    let mut grants = vec![Grant {
        path: worktree.to_path_buf(),
        write: true,
        needed_for: "the worktree itself — without it the pane cannot read or edit any file",
    }];

    for tool in tools {
        // Grant the containing directory, not the exe: a toolchain loads DLLs and
        // helper binaries from beside itself (`git.exe` alone is useless without
        // `libexec/git-core`), so a file-level ACE would produce a tool that
        // starts and then fails on its first subprocess.
        let Some(dir) = tool_grant_root(tool) else {
            continue;
        };
        if is_already_reachable(&dir) || grants.iter().any(|g| g.path == dir) {
            continue;
        }
        grants.push(Grant {
            path: dir,
            write: false,
            needed_for: "a toolchain the pane runs (git, the shell); read+execute only",
        });
    }

    AppContainerPlan {
        profile: profile_name(&worktree.to_string_lossy()),
        grants,
        capabilities: capabilities_for(network),
    }
}

/// The directory to grant for a tool at `exe`.
///
/// Climbs out of a `bin/` (or `cmd/`, git-for-Windows' spelling) so the grant
/// covers the whole installation rather than only its entry points — the
/// `libexec` problem above.
fn tool_grant_root(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let leaf = dir.file_name()?.to_string_lossy().to_ascii_lowercase();
    if matches!(leaf.as_str(), "bin" | "cmd" | "mingw64" | "usr") {
        dir.parent().map(Path::to_path_buf)
    } else {
        Some(dir.to_path_buf())
    }
}

/// Directories every AppContainer can already read, so granting is unnecessary.
///
/// `System32` (and the Windows directory generally) ships an
/// `ALL APPLICATION PACKAGES` ACE — measured: `cmd.exe` runs inside a container
/// untouched, while `C:\Program Files\Git` does not.
fn is_already_reachable(dir: &Path) -> bool {
    let s = dir
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('\\', "/");
    s.contains("/windows/system32") || s.ends_with("/windows") || s.contains("/windows/syswow64")
}

/// The `icacls` argv that applies one grant to `sid`.
///
/// `icacls` rather than `SetNamedSecurityInfoW` follows the same reasoning as
/// [`crate::fsperm`]: it ships with Windows, and it keeps this crate free of
/// Win32 plumbing so the command shape stays unit-tested here while the host —
/// the only crate that links `windows-sys` — derives the SID and runs it.
///
/// `*<SID>` is icacls' spelling for "this is a SID, not an account name": an
/// AppContainer SID has no resolvable name, so without the star icacls rejects
/// it as an unknown principal.
pub fn icacls_argv(grant: &Grant, sid: &str) -> Vec<String> {
    vec![
        grant.path.to_string_lossy().into_owned(),
        "/grant".to_string(),
        format!("*{sid}:{}", grant.icacls_perms()),
        // Apply to the tree without walking it twice, and stay quiet on success.
        "/T".to_string(),
        "/C".to_string(),
        "/Q".to_string(),
    ]
}

/// The command a user can run themselves when thegn could not apply a grant.
///
/// Printed verbatim by `doctor`, so it must be pasteable: quoted path, and the
/// same permissions thegn would have set.
pub fn manual_grant_hint(grant: &Grant, sid: &str) -> String {
    format!(
        "icacls \"{}\" /grant *{sid}:{} /T",
        grant.path.display(),
        grant.icacls_perms()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_name_is_deterministic_and_fits_windows() {
        let a = profile_name(r"C:\Users\u\wt");
        assert_eq!(a, profile_name(r"C:\Users\u\wt"), "must be deterministic");
        assert!(a.len() <= MAX_PROFILE_NAME);
        assert!(a.starts_with("thegn-"));
    }

    #[test]
    fn deep_sibling_worktrees_do_not_collide() {
        // The realistic collision: siblings under one repo share a long prefix,
        // so plain truncation would give them the SAME container identity — and
        // therefore the same grants and the same teardown.
        let base = r"C:\Users\u\Documents\GitHub\some-rather-long-repository-name\.worktrees";
        let a = profile_name(&format!(r"{base}\feature-alpha-one"));
        let b = profile_name(&format!(r"{base}\feature-alpha-two"));
        assert!(a.len() <= MAX_PROFILE_NAME && b.len() <= MAX_PROFILE_NAME);
        assert_ne!(a, b, "two worktrees must never share a profile");
    }

    #[test]
    fn network_maps_to_capability_sids() {
        // No capability at all is a real "no network" — stronger than a flag.
        assert!(capabilities_for(Network::None).is_empty());
        assert_eq!(capabilities_for(Network::Nat), vec!["internetClient"]);
        // Host opens the LAN too; Nat deliberately does not.
        let host = capabilities_for(Network::Host);
        assert!(host.contains(&"internetClient"));
        assert!(host.contains(&"privateNetworkClientServer"));
        assert!(!capabilities_for(Network::Nat).contains(&"privateNetworkClientServer"));
    }

    #[test]
    fn the_worktree_is_always_granted_writable() {
        let p = plan(Path::new(r"C:\Users\u\wt"), Network::None, &[]);
        assert_eq!(p.grants.len(), 1);
        assert_eq!(p.grants[0].path, PathBuf::from(r"C:\Users\u\wt"));
        assert!(p.grants[0].write);
        assert_eq!(p.grants[0].icacls_perms(), "(OI)(CI)(M)");
    }

    #[test]
    fn a_toolchain_grants_its_install_root_not_its_bin() {
        // `git.exe` alone is useless without `libexec/git-core` beside it.
        let p = plan(
            Path::new(r"C:\Users\u\wt"),
            Network::None,
            &[PathBuf::from(r"C:\Program Files\Git\cmd\git.exe")],
        );
        let g = p.grants.iter().find(|g| !g.write).expect("toolchain grant");
        assert_eq!(g.path, PathBuf::from(r"C:\Program Files\Git"));
        assert_eq!(
            g.icacls_perms(),
            "(OI)(CI)(RX)",
            "read+execute, never write"
        );
    }

    #[test]
    fn system32_is_not_granted() {
        // It already carries an ALL APPLICATION PACKAGES ACE. Granting it would
        // be an unnecessary change to a system directory.
        let p = plan(
            Path::new(r"C:\Users\u\wt"),
            Network::None,
            &[PathBuf::from(r"C:\WINDOWS\system32\cmd.exe")],
        );
        assert_eq!(p.grants.len(), 1, "only the worktree: {:?}", p.grants);
    }

    #[test]
    fn icacls_argv_marks_the_principal_as_a_sid() {
        let g = Grant {
            path: PathBuf::from(r"C:\Users\u\wt"),
            write: true,
            needed_for: "test",
        };
        let argv = icacls_argv(&g, "S-1-15-2-1-2-3");
        // The leading `*` is not decoration: an AppContainer SID has no
        // resolvable account name, so without it icacls rejects the principal.
        assert!(
            argv.iter().any(|a| a == "*S-1-15-2-1-2-3:(OI)(CI)(M)"),
            "{argv:?}"
        );
        assert_eq!(argv[0], r"C:\Users\u\wt");
        assert!(argv.iter().any(|a| a == "/T"), "must apply to the tree");
    }

    #[test]
    fn the_manual_hint_is_pasteable() {
        let g = Grant {
            path: PathBuf::from(r"C:\Program Files\Git"),
            write: false,
            needed_for: "test",
        };
        let hint = manual_grant_hint(&g, "S-1-15-2-9");
        // The path has a space in it; an unquoted hint would be wrong advice.
        assert!(hint.contains(r#""C:\Program Files\Git""#), "{hint}");
        assert!(hint.contains("*S-1-15-2-9:(OI)(CI)(RX)"), "{hint}");
    }

    #[test]
    fn duplicate_tools_grant_once() {
        let p = plan(
            Path::new(r"C:\Users\u\wt"),
            Network::None,
            &[
                PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"),
                PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
            ],
        );
        assert_eq!(p.grants.len(), 2, "one worktree + one install root");
    }
}
