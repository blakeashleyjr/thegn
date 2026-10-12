//! Host-toolchain, cache, and writable-carve-out mount builders for the local
//! (bwrap/systemd/OCI) sandbox backends.
//!
//! Extracted from `sandbox.rs` (which is at its god-file ratchet ceiling). These
//! functions turn "reuse the host toolchain" into a concrete list of
//! path-preserving [`Mount`]s: the read-only substrate ($HOME dotfiles,
//! `/nix/store`, FHS dirs, identity files), the read-write build caches, and —
//! under a read-only `$HOME` (the default hardened profile) — a narrow set of
//! read-write carve-outs so shell/tool state keeps working.

use crate::config::SandboxProfile;
use crate::sandbox::{Backend, BackendFamily, Mount, SandboxSpec};
use std::path::{Path, PathBuf};

/// Mounts that bring the host toolchain into an OCI container so the user's
/// real shell, dotfiles, and tools work identically inside the sandbox.
///
/// This is most useful on NixOS, where everything lives in `/nix/store` and
/// `/run/current-system/sw`, but the same logic also picks up conventional
/// FHS paths (`/usr`, `/lib`, `/bin`) on non-NixOS hosts.
///
/// Only paths that **exist on the host** at spec-build time are included —
/// the list is always a subset of what's actually present, never a wish list.
/// All mounts are **read-only** (the container should not modify host system
/// files).
/// `ro_home`: mount `$HOME` read-only (OCI, or any read-only-root profile) or
/// read-write (bwrap under `profile = "open"`).
/// See the comment on the home-directory section below.
pub fn host_toolchain_mounts() -> Vec<Mount> {
    host_toolchain_mounts_ro_home(true) // public API defaults to safe (ro)
}

pub fn host_toolchain_mounts_ro_home(ro_home: bool) -> Vec<Mount> {
    host_toolchain_mounts_for_home(Some(ro_home))
}

/// The host-toolchain substrate with a sealed (private) `$HOME`: everything
/// [`host_toolchain_mounts_ro_home`] emits **except** the ambient `$HOME`, plus
/// the reviewed [`sealed_home_allowlist`] entries. The caller pairs this with a
/// tmpfs `$HOME` (bwrap `--tmpfs`, systemd `ProtectHome=tmpfs`; OCI never
/// mounted it) so nothing outside the allowlist exists in the guest.
pub fn host_toolchain_mounts_sealed_home() -> Vec<Mount> {
    let mut mounts = host_toolchain_mounts_for_home(None);
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        mounts.extend(sealed_home_allowlist(Path::new(&home)));
    }
    mounts
}

/// `home_ro`: `Some(ro)` mounts the ambient `$HOME` (read-only or read-write);
/// `None` withholds it entirely (sealed tiers).
fn host_toolchain_mounts_for_home(home_ro: Option<bool>) -> Vec<Mount> {
    let mut mounts = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();

    let ro = |path: &str| Mount {
        host: path.to_string(),
        dest: path.to_string(),
        ro: true,
        cache: false,
    };

    let exists = |p: &str| std::path::Path::new(p).exists();

    // ── NixOS / Nix-on-anything paths ───────────────────────────────────────
    // /nix/store  — every binary, library, and config file Nix manages lives
    //               here. Mounting it ro brings in the shell ($SHELL resolves
    //               to a store path), starship, completions, dotfile symlink
    //               targets, etc. without any per-package enumeration.
    if exists("/nix/store") {
        mounts.push(ro("/nix/store"));
    }
    // /run/current-system — the stable generation symlinks:
    //   sw/bin/zsh, sw/share/zsh, etc. The container's $SHELL will resolve
    //   correctly once /nix/store is present.
    if exists("/run/current-system") {
        mounts.push(ro("/run/current-system"));
    }
    // /nix/var/nix/profiles — user profiles (alternative to per-user path).
    if exists("/nix/var/nix/profiles") {
        mounts.push(ro("/nix/var/nix/profiles"));
    }
    // /etc/profiles/per-user/<user> — per-user packages installed by
    // home-manager (e.g. zsh plugins, starship when not in system profile).
    if !home.is_empty() {
        let username = std::path::Path::new(&home)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if !username.is_empty() {
            let p = format!("/etc/profiles/per-user/{username}");
            if exists(&p) {
                mounts.push(ro(&p));
            }
        }
    }
    // /etc/static — NixOS-managed /etc entries (zshrc, zshenv, zprofile, …).
    if exists("/etc/static") {
        mounts.push(ro("/etc/static"));
    }

    // ── Conventional FHS paths (non-NixOS, or mixed systems) ────────────────
    // These are absent on pure NixOS (everything is in /nix) but present on
    // Ubuntu/Debian/Fedora/Arch and WSL; include them when they exist.
    for path in &["/usr", "/lib", "/lib64", "/bin"] {
        // Skip /bin and /lib if they're just symlinks into /usr (common on
        // modern FHS systems) to avoid duplicate mounts.
        let p = std::path::Path::new(path);
        if p.exists() && !p.is_symlink() {
            mounts.push(ro(path));
        }
    }

    // ── Identity/locale files every process expects ──────────────────────────
    // passwd/group are needed for getpwuid() (shell prompts, git author, etc.)
    // Overlaying the host files means the container sees the real username.
    for path in &[
        "/etc/passwd",
        "/etc/group",
        "/etc/hosts",
        "/etc/localtime",
        "/etc/resolv.conf",
        "/etc/zshrc", // NixOS system-wide zsh init (sourced by /etc/static/zshrc)
        "/etc/zshenv",
        "/etc/zprofile",
    ] {
        if exists(path) {
            mounts.push(ro(path));
        }
    }

    // ── User home directory (dotfiles) ───────────────────────────────────────
    // Mount $HOME so ~/.zshrc, ~/.config/starship.toml, ~/.gitconfig and similar
    // dotfiles are visible. On NixOS these are symlinks into /nix/store, so this
    // mount is complementary: symlink + /nix/store (target).
    //
    // ro_home controls read-only vs read-write:
    //   OCI (podman/docker) — always ro: the container runs as root in a foreign
    //     image; we expose dotfiles for reading but must not let root write to
    //     the host.
    //   bwrap/systemd under a read-only-root profile (hardened/sealed, the
    //     default) — ro: the whole point is that a sandboxed process can't reach
    //     out of the worktree and modify/delete host files. Writes tools expect
    //     (zsh history, zoxide, atuin) are carved back narrowly by
    //     `default_writable_carveouts()` (and by `[sandbox] mounts`).
    //   bwrap under `profile = "open"` — rw: the escape hatch, full host-parity
    //     writable $HOME.
    if let Some(ro_home) = home_ro
        && !home.is_empty()
        && exists(&home)
    {
        mounts.push(Mount {
            host: home.clone(),
            dest: home,
            ro: ro_home,
            cache: false,
        });
    }

    mounts
}

pub fn auto_cache_mounts() -> Vec<Mount> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }
    let candidates = [
        ".cargo/registry",
        ".cargo/git",
        ".rustup",
        ".npm",
        ".cache/pnpm",
        ".cache/yarn",
        "go/pkg/mod",
        ".cache/go-build",
        ".cache/pip",
        ".cache/uv",
        ".m2/repository",
        ".gradle/caches",
        // Compile + pre-commit-hook-framework caches so `git commit` hooks run
        // (prek/pre-commit write hook envs; sccache writes objects) instead of
        // dying "Read-only file system" under a hardened read-only $HOME.
        ".cache/sccache",
        ".cache/prek",
        ".cache/pre-commit",
        // Nix's client-side caches remain an explicit target-side convenience;
        // they do not authorize host evaluation or repository `.direnv` writes.
        ".cache/nix",
    ];
    candidates
        .iter()
        .filter_map(|rel| {
            let p = std::path::Path::new(&home).join(rel);
            p.is_dir().then(|| {
                let s = p.to_string_lossy().into_owned();
                Mount {
                    host: s.clone(),
                    dest: s,
                    ro: false,
                    cache: true,
                }
            })
        })
        .collect()
}

/// Narrow read-write paths carved back into an otherwise read-only `$HOME` so
/// shell/tool state keeps persisting under the default hardened profile. Only
/// paths that **exist** on the host are returned — directories *and* files
/// (bwrap needs the mountpoint to exist so it overmounts the ro `$HOME` inode
/// rather than trying to create it).
///
/// `/tmp` is intentionally absent — it is already writable on every backend
/// (bwrap `--tmpfs /tmp`, OCI `--tmpfs /tmp`, systemd `PrivateTmp=yes`). Users
/// extend this via `[sandbox] mounts` (e.g. `~/.gnupg`, a custom history file);
/// the resolve-time covered-check ([`keep_cfg_mount`]) lets a read-write
/// directory *or* existing file overmount the read-only `$HOME`.
///
/// `~/.keychain` (which stores shell scripts your host login shells later
/// *source*) is carved only for non-sealed profiles: a writable `~/.keychain`
/// from the sealed agent profile would be a persistence vector into the host
/// session — exactly what the read-only `$HOME` exists to prevent.
pub fn default_writable_carveouts(profile: SandboxProfile) -> Vec<Mount> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    default_writable_carveouts_at(profile, &home, &|key| std::env::var_os(key))
}

/// Why an approved XDG application carve-out was omitted. Paths are returned
/// separately so callers can report the affected root or leaf without exposing
/// file contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdgOmission {
    InvalidRoot,
    RelativeRoot,
    MissingRoot,
    SymlinkedRoot,
    NotDirectory,
    UnresolvableRoot,
    UnresolvableParent,
    OverlappingRoots,
    UnresolvableLeaf,
    LeafEscapesRoot,
    LeafNotDirectory,
    ProtectedState,
    ProtectedRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XdgOmissionEntry {
    pub path: PathBuf,
    pub reason: XdgOmission,
}

impl XdgOmission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRoot => "invalid or non-UTF-8 XDG root",
            Self::RelativeRoot => "XDG root is not absolute",
            Self::MissingRoot => "explicit XDG root is missing or unreadable",
            Self::SymlinkedRoot => "XDG root is a symlink",
            Self::NotDirectory => "XDG root is not a directory",
            Self::UnresolvableRoot => "XDG root cannot be canonicalized",
            Self::UnresolvableParent => "XDG root parent cannot be canonicalized",
            Self::OverlappingRoots => "XDG state and data roots overlap",
            Self::UnresolvableLeaf => "approved XDG leaf cannot be canonicalized",
            Self::LeafEscapesRoot => "approved XDG leaf escapes its root",
            Self::LeafNotDirectory => "approved XDG leaf is not a directory",
            Self::ProtectedState => "approved XDG leaf overlaps Thegn state",
            Self::ProtectedRuntime => "approved XDG leaf overlaps Thegn runtime state",
        }
    }
}

/// Return approved writable XDG mounts together with every observable
/// omission. Defaults absent from disk are ordinary and are not reported;
/// explicitly configured roots that are unavailable are reported.
pub fn xdg_carveout_report_at(
    profile: SandboxProfile,
    home: &Path,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> (Vec<Mount>, Vec<XdgOmissionEntry>) {
    if !home.is_absolute() {
        return (Vec::new(), Vec::new());
    }
    // Personal scratch + the XDG state/data dirs where shell history, zoxide,
    // and atuin keep their databases, plus the legacy `$HOME`-root history files
    // many shells still write. Deliberately narrow: source trees, ~/.ssh,
    // ~/.config, and the rest of $HOME stay read-only.
    let mut rels: Vec<&str> = vec!["tmp", ".zsh_history", ".bash_history"];
    // Interactive/hardened only — never the sealed agent:
    //   `~/.keychain` stores scripts host login shells later *source*.
    //   `~/.claude-profiles` is where HOME-swapping coding-agent profile wrappers
    //     (e.g. `claude-profile` / `claude-multiplex`, which `export
    //     HOME=$HOME/.claude-profiles/<name>` then exec the agent) keep each
    //     profile's config + session state. thegn mounts $HOME read-only and
    //     can't predict that per-profile subdir, so the agent's `session-env`
    //     mkdir dies EROFS without this carve. Off for sealed: a network=none
    //     agent must not get write access to the host's real credential dirs.
    //   `~/.claude/projects` is the SHARED base each HOME-swapped profile symlinks
    //     its own `projects/` into — it holds the cross-profile agent memory +
    //     session transcripts the profile wrappers deliberately share. The rest of
    //     `~/.claude` stays read-only (settings/plugins/skills), but this one
    //     subdir must be writable or every profile's transcript/memory write fails
    //     EROFS. Off for sealed for the same reason as above (and so an isolated
    //     agent can't read/write the shared cross-profile memory store).
    if !matches!(
        profile,
        SandboxProfile::Sealed | SandboxProfile::SealedTunnel
    ) {
        rels.push(".keychain");
        rels.push(".claude-profiles");
        rels.push(".claude/projects");
    }
    let mut mounts: Vec<Mount> = rels
        .iter()
        .filter_map(|rel| {
            let p = home.join(rel);
            // Dir or file — but must exist so bwrap overmounts an existing
            // inode (skips dangling symlinks, which `is_dir`/`is_file` reject).
            (p.is_dir() || p.is_file()).then(|| {
                let s = p.to_string_lossy().into_owned();
                Mount {
                    host: s.clone(),
                    dest: s,
                    ro: false,
                    cache: false,
                }
            })
        })
        .collect();
    let mut omissions = Vec::new();
    let state_root = xdg_mount_root(home, "XDG_STATE_HOME", ".local/state", env, &mut omissions);
    let data_root = xdg_mount_root(home, "XDG_DATA_HOME", ".local/share", env, &mut omissions);
    let overlap = state_root
        .as_ref()
        .zip(data_root.as_ref())
        .is_some_and(|((a, _), (b, _))| a == b || a.starts_with(b) || b.starts_with(a));
    if overlap {
        if let Some((root, _)) = state_root.as_ref() {
            omissions.push(XdgOmissionEntry {
                path: root.clone(),
                reason: XdgOmission::OverlappingRoots,
            });
        }
        if let Some((root, _)) = data_root.as_ref() {
            omissions.push(XdgOmissionEntry {
                path: root.clone(),
                reason: XdgOmission::OverlappingRoots,
            });
        }
    } else {
        let state_home = env("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));
        let protected_state = state_home.join("thegn");
        for (root, leaves) in [
            (state_root.as_ref(), &["bash", "zsh"][..]),
            (data_root.as_ref(), &["fish", "zoxide", "atuin"][..]),
        ] {
            if let Some((root, namespace_root)) = root {
                for leaf in leaves {
                    if *leaf == "atuin"
                        && matches!(
                            profile,
                            SandboxProfile::Sealed | SandboxProfile::SealedTunnel
                        )
                    {
                        continue;
                    }
                    let destination = namespace_root.join(leaf);
                    let source_path = root.join(leaf);
                    let Ok(source) = source_path.canonicalize() else {
                        omissions.push(XdgOmissionEntry {
                            path: source_path,
                            reason: XdgOmission::UnresolvableLeaf,
                        });
                        continue;
                    };
                    if !source.starts_with(root) {
                        omissions.push(XdgOmissionEntry {
                            path: source_path,
                            reason: XdgOmission::LeafEscapesRoot,
                        });
                        continue;
                    }
                    if !source.is_dir() {
                        omissions.push(XdgOmissionEntry {
                            path: source_path,
                            reason: XdgOmission::LeafNotDirectory,
                        });
                        continue;
                    }
                    if source.starts_with(&protected_state) || protected_state.starts_with(&source)
                    {
                        omissions.push(XdgOmissionEntry {
                            path: source_path,
                            reason: XdgOmission::ProtectedState,
                        });
                        continue;
                    }
                    if (*leaf == "bash" || *leaf == "zsh")
                        && root
                            .join("thegn/run")
                            .canonicalize()
                            .is_ok_and(|p| p.starts_with(&source))
                    {
                        omissions.push(XdgOmissionEntry {
                            path: source_path,
                            reason: XdgOmission::ProtectedRuntime,
                        });
                        continue;
                    }
                    mounts.push(Mount {
                        host: source.to_string_lossy().into_owned(),
                        dest: destination.to_string_lossy().into_owned(),
                        ro: false,
                        cache: false,
                    });
                }
            }
        }
    }
    // Coding-agent config dirs (Claude Code's `CLAUDE_CONFIG_DIR` / `~/.claude`,
    // Codex's `CODEX_HOME` / `~/.codex`) carved read-write: the agent CLI writes
    // runtime state (`session-env`, todos, shell snapshots) here, so a plain
    // sandbox shell where the user runs `claude`/`codex` *manually* needs it
    // writable too — the bundle-launch path (`bundle::compose_inner`) only carves
    // this when thegn itself recognizes the launched provider, missing the
    // manual-in-a-shell case. `effective_config_dir` returns `Some` only for a
    // dir that already exists (so bwrap overmounts an existing inode). Off for
    // sealed: an untrusted network=none agent must not get write access to the
    // host's real credential dirs (same posture as `~/.keychain` above).
    if !matches!(
        profile,
        SandboxProfile::Sealed | SandboxProfile::SealedTunnel
    ) {
        for p in crate::account::providers() {
            if let Some(dir) = crate::account::effective_config_dir(p) {
                mounts.push(Mount {
                    host: dir.clone(),
                    dest: dir,
                    ro: false,
                    cache: false,
                });
            }
        }
    }
    (mounts, omissions)
}

fn default_writable_carveouts_at(
    profile: SandboxProfile,
    home: &Path,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<Mount> {
    let (mounts, _) = xdg_carveout_report_at(profile, home, env);
    mounts
}

fn xdg_mount_root(
    home: &Path,
    variable: &str,
    fallback: &str,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    omissions: &mut Vec<XdgOmissionEntry>,
) -> Option<(PathBuf, PathBuf)> {
    let configured = env(variable);
    let root = match configured.as_ref() {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.as_os_str().is_empty() || path.to_str().is_none() {
                omissions.push(XdgOmissionEntry {
                    path,
                    reason: XdgOmission::InvalidRoot,
                });
                return None;
            }
            if !path.is_absolute() {
                omissions.push(XdgOmissionEntry {
                    path,
                    reason: XdgOmission::RelativeRoot,
                });
                return None;
            }
            path
        }
        None => home.join(fallback),
    };
    let metadata = match std::fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(_) => {
            if configured.is_some() {
                omissions.push(XdgOmissionEntry {
                    path: root,
                    reason: XdgOmission::MissingRoot,
                });
            }
            return None;
        }
    };
    if metadata.file_type().is_symlink() {
        omissions.push(XdgOmissionEntry {
            path: root,
            reason: XdgOmission::SymlinkedRoot,
        });
        return None;
    }
    if !metadata.is_dir() {
        omissions.push(XdgOmissionEntry {
            path: root,
            reason: XdgOmission::NotDirectory,
        });
        return None;
    }
    let canonical = match root.canonicalize() {
        Ok(path) => path,
        Err(_) => {
            omissions.push(XdgOmissionEntry {
                path: root,
                reason: XdgOmission::UnresolvableRoot,
            });
            return None;
        }
    };
    let parent = match root.parent().and_then(|path| path.canonicalize().ok()) {
        Some(path) => path,
        None => {
            omissions.push(XdgOmissionEntry {
                path: root,
                reason: XdgOmission::UnresolvableParent,
            });
            return None;
        }
    };
    if !root
        .file_name()
        .is_some_and(|file_name| canonical == parent.join(file_name))
    {
        omissions.push(XdgOmissionEntry {
            path: root,
            reason: XdgOmission::UnresolvableRoot,
        });
        None
    } else {
        Some((canonical, root))
    }
}

/// Decide whether a mount `m` should be added given the mounts already
/// assembled in `existing`. Returns `true` to keep it.
///
/// The tightest already-mounted parent wins:
/// - not covered by any parent → keep;
/// - exact duplicate of an existing mount → drop;
/// - a read-write **directory or existing file** strictly under a **read-only**
///   parent → keep: it overmounts the parent read-write (e.g. `~/tmp` or
///   `~/.gnupg`, or a history *file* like `~/.zsh_history`, under a read-only
///   `$HOME`). The mountpoint already exists inside the ro parent bind, so bwrap
///   binds over the existing inode without creating anything. This is what makes
///   the writable carve-outs work;
/// - anything else already covered (a read-only entry, a read-write path under
///   a read-write parent, or a mountpoint that doesn't exist on the host so
///   bwrap can't create it inside the ro parent) → drop.
pub fn keep_cfg_mount(existing: &[Mount], m: &Mount) -> bool {
    let mpath = std::path::Path::new(&m.host);
    let parent = existing
        .iter()
        .filter(|e| mpath.starts_with(&e.host))
        .max_by_key(|e| e.host.len());
    match parent {
        None => true,
        Some(p) if p.host == m.host => false,
        Some(p) => !m.ro && p.ro && (mpath.is_dir() || mpath.is_file()),
    }
}

/// Parse one `[sandbox].mounts` entry — `"host"`, `"host:ro"`, `"host:dest"`,
/// `"host:dest:ro"`, or the `cache` variants — into a [`Mount`]. `~/`-prefixed
/// paths are tilde-expanded so the spec is a valid filesystem path (bwrap does
/// no shell expansion). Extracted from `sandbox.rs` (god-file ratchet ceiling).
pub(crate) fn parse_mount(spec: &str) -> Mount {
    // "host", "host:ro", or "host:dest" / "host:dest:ro".
    // Paths starting with "~/" are expanded to the real home directory so the
    // mount spec is valid as a filesystem path (bwrap does not do shell expansion).
    let expand = |p: &str| crate::util::expand_tilde(p);
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        [host] => Mount {
            host: expand(host),
            dest: expand(host),
            ro: false,
            cache: false,
        },
        [host, "ro"] => Mount {
            host: expand(host),
            dest: expand(host),
            ro: true,
            cache: false,
        },
        [host, "cache"] => Mount {
            host: expand(host),
            dest: expand(host),
            ro: false,
            cache: true,
        },
        [host, dest] => Mount {
            host: expand(host),
            dest: expand(dest),
            ro: false,
            cache: false,
        },
        [host, dest, "ro"] => Mount {
            host: expand(host),
            dest: expand(dest),
            ro: true,
            cache: false,
        },
        [host, dest, "cache"] => Mount {
            host: expand(host),
            dest: expand(dest),
            ro: false,
            cache: true,
        },
        _ => Mount {
            host: expand(spec),
            dest: expand(spec),
            ro: false,
            cache: false,
        },
    }
}

/// How much of the host `$HOME` a resolved sandbox can see. Reported by
/// `thegn doctor`; **confidentiality is its own property** — a read-only
/// `$HOME` protects integrity only, every readable file is still exfiltratable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeView {
    /// Host `$HOME` is not visible (private tmpfs / never mounted); only an
    /// explicit allowlist is bound in.
    Hidden,
    /// The whole host `$HOME` is readable, not writable (`hardened`).
    ReadOnly,
    /// The whole host `$HOME` is readable and writable (`open`, host process).
    Writable,
}

impl HomeView {
    pub fn as_str(self) -> &'static str {
        match self {
            HomeView::Hidden => "hidden",
            HomeView::ReadOnly => "read-only",
            HomeView::Writable => "writable",
        }
    }
}

/// Backends that can hide `$HOME` for a sealed profile. WSL is an OCI-family
/// *reserved* kind with no runtime behind it: it cannot be shown to hide
/// anything, so it fails the sealed floor rather than being assumed private.
fn backend_can_hide_home(backend: Backend) -> bool {
    backend != Backend::Wsl
        && matches!(
            backend.profile().family,
            BackendFamily::Oci | BackendFamily::Bwrap | BackendFamily::Systemd
        )
}

/// Roots `ProtectHome=tmpfs` actually covers; a `$HOME` elsewhere stays visible.
const SYSTEMD_PROTECT_HOME_ROOTS: &[&str] = &["/home", "/root", "/run/user"];

fn systemd_protect_home_covers(home: &Path) -> bool {
    let covered = |p: &Path| SYSTEMD_PROTECT_HOME_ROOTS.iter().any(|r| p.starts_with(r));
    covered(home) && home.canonicalize().map(|c| covered(&c)).unwrap_or(true)
}

/// What a *planned* launch under `profile` on `backend` shows of `$HOME` (doctor,
/// before any spec exists). Mirrors [`home_view`], which answers for a real spec.
pub fn planned_home_view(profile: SandboxProfile, backend: Backend) -> HomeView {
    let family = backend.profile().family;
    if profile.hides_home() && backend_can_hide_home(backend) {
        // `ProtectHome=tmpfs` only reaches /home, /root and /run/user: a systemd
        // launch from a `$HOME` elsewhere is refused, so it is not "hidden".
        let systemd_misses_home = family == BackendFamily::Systemd
            && std::env::var_os("HOME")
                .map(|h| !systemd_protect_home_covers(Path::new(&h)))
                .unwrap_or(true);
        if !systemd_misses_home {
            return HomeView::Hidden;
        }
    }
    match family {
        BackendFamily::Oci | BackendFamily::Bwrap | BackendFamily::Systemd
            if profile.read_only_root() =>
        {
            HomeView::ReadOnly
        }
        _ => HomeView::Writable,
    }
}

/// What the resolved `spec` actually shows of `home` — derived from the mounts
/// it will emit, not from the profile, so a stray `[sandbox] mounts = ["~"]` or
/// `file_access = "all"` is reported as the exposure it is.
pub fn home_view(spec: &SandboxSpec, home: &Path) -> HomeView {
    let family = spec.backend.profile().family;
    if spec.backend == Backend::Wsl {
        return HomeView::Writable;
    }
    // The tightest mount that is an ancestor-or-equal of `$HOME`; shared by
    // bwrap, OCI and sealed systemd (which binds the allowlist back in).
    let covering = spec
        .mounts
        .iter()
        .filter(|m| home.starts_with(&m.dest))
        .max_by_key(|m| m.dest.len())
        .map(|m| {
            if m.ro {
                HomeView::ReadOnly
            } else {
                HomeView::Writable
            }
        });
    match family {
        BackendFamily::Systemd => {
            if spec.seal_home.is_some() {
                if let Some(v) = covering {
                    v
                } else if !spec.read_only_root {
                    // `ProtectHome=tmpfs` is only emitted under read_only_root.
                    HomeView::Writable
                } else if systemd_protect_home_covers(home) {
                    HomeView::Hidden
                } else if spec.read_only_root {
                    // `ProtectHome=tmpfs` does not reach this `$HOME`: fail closed.
                    HomeView::ReadOnly
                } else {
                    HomeView::Writable
                }
            } else if spec.read_only_root {
                HomeView::ReadOnly
            } else {
                HomeView::Writable
            }
        }
        BackendFamily::Oci | BackendFamily::Bwrap => {
            if family == BackendFamily::Bwrap
                && matches!(
                    spec.file_access,
                    crate::config::FileAccess::All | crate::config::FileAccess::Host
                )
            {
                return HomeView::Writable;
            }
            covering.unwrap_or(HomeView::Hidden)
        }
        _ => HomeView::Writable,
    }
}

/// `$HOME`-relative paths that must never reach a sealed sandbox, directly or
/// through a symlink. Matched against the *resolved* target.
const SEALED_HOME_DENY: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".secrets",
    ".password-store",
    ".netrc",
    ".docker",
    ".kube",
    ".config/gh",
    ".config/gcloud",
    ".config/doctl",
    ".config/op",
    ".config/sops",
    ".config/age",
    ".config/BraveSoftware",
    ".config/google-chrome",
    ".config/chromium",
    ".mozilla",
    ".local/share/keyrings",
    ".local/share/atuin",
    ".local/state/thegn",
    ".local/share/thegn",
    ".claude",
    ".claude-profiles",
    ".codex",
    ".gemini",
    ".git-credentials",
    ".config/git/credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".vault-token",
    ".config/rclone",
    ".terraform.d",
    ".docker/config.json",
];

/// Absolute roots that are secret by convention wherever they are linked from.
const SEALED_ABS_DENY: &[&str] = &["/run/agenix", "/run/secrets", "/run/user", "/root"];

/// Absolute roots a resolved allowlist target may live under besides `$HOME`
/// itself (home-manager and similar link dotfiles into the Nix store).
const SEALED_ABS_APPROVED: &[&str] = &["/nix/store", "/usr", "/etc"];

/// Dotfiles/config a sealed pane keeps read-only so the user's real shell, git
/// identity and prompt work. Reviewed list — adding a credential-bearing path
/// here is a security change. Extend per-user via `[sandbox] mounts`.
///
/// Single **files** only for `git` and `zsh` config dirs: those directories hold
/// credentials by convention (`~/.config/git/credentials`, `~/.zsh_history`),
/// so they are never bound wholesale. The allowlist binds **files only**
/// (`is_file()` after canonicalization); there is no directory entry.
const SEALED_HOME_ALLOW: &[&str] = &[
    ".gitconfig",
    ".config/git/config",
    ".config/git/ignore",
    ".config/git/attributes",
    ".config/git/allowed_signers",
    ".zshenv",
    ".zshrc",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".config/zsh/.zshenv",
    ".config/zsh/.zshrc",
    ".config/zsh/.zprofile",
    ".config/zsh/.zlogin",
    ".config/zsh/.zlogout",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".inputrc",
    ".config/starship.toml",
];

/// Whether the resolved `target` may be exposed in a sealed `$HOME`: under an
/// approved non-secret root (the home itself minus the deny list, or the Nix
/// store / `/usr` / `/etc`), not under a denied path, and not an *ancestor* of
/// one (a link to `~/.config` would drag `~/.config/gh` in with it).
fn sealed_target_ok(target: &Path, home: &Path) -> bool {
    !sealed_deny_hit(target, &deny_homes(home))
        && (target.starts_with(home) || SEALED_ABS_APPROVED.iter().any(|r| target.starts_with(r)))
}

/// Home directories the deny list is resolved against: `$HOME` (both spellings)
/// plus the account's passwd home when it differs. The passwd home is read from
/// `/etc/passwd` for `$USER`/`$LOGNAME` (no libc/cfg; best-effort, absent where
/// there is no passwd file).
fn deny_homes(home: &Path) -> Vec<PathBuf> {
    let mut homes = vec![home.to_path_buf()];
    if let Ok(c) = home.canonicalize() {
        homes.push(c);
    }
    let names: Vec<String> = ["USER", "LOGNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .filter(|v| !v.is_empty())
        .collect();
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() >= 6 && names.iter().any(|n| n == f[0]) && f[5].starts_with('/') {
                let p = PathBuf::from(f[5]);
                if let Ok(c) = p.canonicalize()
                    && !homes.contains(&c)
                {
                    homes.push(c);
                }
                if !homes.contains(&p) {
                    homes.push(p);
                }
            }
        }
    }
    homes
}

/// Every deny entry (absolute) for the given home spellings, including thegn's
/// own state dir wherever `XDG_STATE_HOME` puts it.
fn deny_entries(homes: &[PathBuf]) -> Vec<PathBuf> {
    homes
        .iter()
        .flat_map(|h| SEALED_HOME_DENY.iter().map(move |d| h.join(d)))
        .chain(SEALED_ABS_DENY.iter().map(PathBuf::from))
        .chain(std::iter::once(crate::util::xdg_state_home().join("thegn")))
        .collect()
}

/// The one named exception to the deny list: the MANAGED per-account credential
/// dirs thegn itself creates (`$XDG_STATE_HOME/thegn/accounts/<provider>/<slug>`)
/// — strictly below the root, never the root, never the ambient `~/.claude` /
/// `~/.codex` fallback or an adopted account dir.
fn managed_account_exempt(target: &Path) -> bool {
    let root = crate::account::managed_root();
    let mut roots = vec![root.clone()];
    if let Ok(c) = root.canonicalize() {
        roots.push(c);
    }
    roots.iter().any(|r| target != r && target.starts_with(r))
}

/// `target` is, or is under, a deny entry.
fn deny_inside(target: &Path, entries: &[PathBuf]) -> bool {
    !managed_account_exempt(target) && entries.iter().any(|d| target.starts_with(d))
}

/// `target` is, or is an ancestor of, a deny entry (it would drag it in).
fn deny_covers(target: &Path, entries: &[PathBuf]) -> bool {
    !managed_account_exempt(target) && entries.iter().any(|d| d.starts_with(target))
}

fn sealed_deny_hit(target: &Path, homes: &[PathBuf]) -> bool {
    let e = deny_entries(homes);
    deny_inside(target, &e) || deny_covers(target, &e)
}

/// Whether a path would be unsafe in a systemd `BindPaths=`/OCI `-v` spec, which
/// are unquoted `a:b[:opts]` strings.
pub fn mount_path_unsafe(p: &str) -> bool {
    p.chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ':' | '"' | '\'' | '\\'))
}

/// Why `p` cannot be placed verbatim in a systemd path property
/// (`BindPaths=`, `BindReadOnlyPaths=`, `ReadWritePaths=`), or `None` if it is
/// safe. systemd splits these values on whitespace, treats `:` as the
/// source/destination/options delimiter, honours quotes and backslash escapes,
/// expands `%` specifiers, and gives a leading `-`/`+` special meaning. Rather
/// than encode (and trust our reading of) each rule, anything that is not a
/// plain absolute, lexically-normalised path is refused.
pub fn systemd_path_refusal(p: &str) -> Option<&'static str> {
    if p.is_empty() {
        return Some("is empty");
    }
    if !p.starts_with('/') {
        return Some("is not absolute (a leading `-`/`+` or a relative path changes meaning)");
    }
    if mount_path_unsafe(p) {
        return Some("contains whitespace, a control character, a quote, a backslash or ':'");
    }
    if p.contains('%') {
        return Some("contains '%', which systemd expands as a specifier");
    }
    let body = p.strip_suffix('/').unwrap_or(p);
    if body.is_empty() {
        return None; // "/"
    }
    if body[1..].split('/').any(|c| matches!(c, "" | "." | "..")) {
        return Some("is not normalised (empty, `.` or `..` component)");
    }
    None
}

/// `FLAG=host:dest` for a systemd bind property, or the reason the pair cannot
/// be represented. The single encoder every systemd mount goes through.
pub fn systemd_bind_property(flag: &str, host: &str, dest: &str) -> Result<String, String> {
    for p in [host, dest] {
        if let Some(why) = systemd_path_refusal(p) {
            return Err(format!("mount path `{p}` {why}"));
        }
    }
    Ok(format!("{flag}={host}:{dest}"))
}

/// `PROP=path` for a single-path systemd property (`ReadWritePaths=`).
pub fn systemd_path_property(prop: &str, path: &str) -> Result<String, String> {
    match systemd_path_refusal(path) {
        Some(why) => Err(format!("mount path `{path}` {why}")),
        None => Ok(format!("{prop}={path}")),
    }
}

/// The directory that must be bound read-only for `link` (normally
/// `/etc/resolv.conf`) to resolve inside a bwrap sandbox whose `/etc` is bound
/// but whose `/run` is not (THE-730). On a systemd-resolved host the chain is
/// `/etc/resolv.conf -> /etc/static/resolv.conf -> /run/systemd/resolve/
/// stub-resolv.conf`, which dangles in the sandbox without that directory.
///
/// Fail closed: `None` unless the fully-resolved target lies strictly below
/// `runtime_root` (`/run`), in a directory at least one level below it, and
/// outside every `substrate` path already bound. Only the target's parent
/// directory is returned, never `runtime_root` itself.
pub fn resolver_bind_dir(link: &Path, runtime_root: &Path, substrate: &[&str]) -> Option<PathBuf> {
    let target = link.canonicalize().ok()?;
    let dir = target.parent()?;
    if dir == runtime_root || !dir.starts_with(runtime_root) {
        return None;
    }
    // Only expose directories used by known host resolver implementations.
    // In particular, never make arbitrary /run state such as per-user sockets
    // visible inside the sandbox.
    let allowed = [
        runtime_root.join("systemd/resolve"),
        runtime_root.join("NetworkManager"),
        runtime_root.join("resolvconf"),
        runtime_root.join("connman"),
    ];
    if !allowed.iter().any(|path| dir == path) {
        return None;
    }
    if substrate.iter().any(|s| dir.starts_with(s)) {
        return None;
    }
    Some(dir.to_path_buf())
}

/// Canonicalize `p`, tolerating a missing tail (canonicalize the deepest
/// existing ancestor, re-append the rest) so a not-yet-created destination is
/// still resolved through any symlinked parent.
fn resolve_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut tail = Vec::new();
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(c) = parent.canonicalize() {
            return tail.iter().rev().fold(c, |acc, n| acc.join(n));
        }
        cur = parent;
    }
    p.to_path_buf()
}

/// Each spelling of a mount path (lexical and canonicalized) for deny checks.
fn spellings(p: &str) -> [PathBuf; 2] {
    let raw = PathBuf::from(p);
    let resolved = resolve_lenient(&raw);
    [raw, resolved]
}

/// Whether a user/default `[sandbox] mounts` entry (host path AND destination,
/// lexically and canonicalized) reaches the sealed deny list — inside it OR as
/// an ancestor of it. Sealed panes drop such entries: `mounts` is the extension
/// point for the allowlist, never a way to bring `~/.gnupg` or `~/.ssh` back.
pub fn sealed_denies_mount(m: &Mount, home: &Path) -> bool {
    let homes = deny_homes(home);
    [&m.host, &m.dest]
        .iter()
        .any(|p| spellings(p).iter().any(|s| sealed_deny_hit(s, &homes)))
}

/// A mount whose host path or destination lies inside the deny list (the final
/// argv builders drop these).
pub fn sealed_mount_inside_deny(m: &Mount, home: &Path) -> bool {
    let e = deny_entries(&deny_homes(home));
    [&m.host, &m.dest]
        .iter()
        .any(|p| spellings(p).iter().any(|s| deny_inside(s, &e)))
}

/// A mount whose HOST path is, or is an ancestor of, a deny entry — the whole
/// tree (secrets included) would be exposed. The gate refuses these launches.
pub fn sealed_mount_covers_deny(m: &Mount, home: &Path) -> bool {
    let e = deny_entries(&deny_homes(home));
    spellings(&m.host).iter().any(|s| deny_covers(s, &e))
}

/// The `[sandbox] mounts` entries a sealed launch drops, as host paths (never
/// contents). Used by the resolver and by `thegn doctor`.
pub fn sealed_dropped_cfg_mounts(cfg_mounts: &[String], home: &Path) -> Vec<String> {
    cfg_mounts
        .iter()
        .map(|m| parse_mount(m))
        .filter(|m| sealed_denies_mount(m, home))
        .map(|m| m.host)
        .collect()
}

/// Defence against a symlink swapped in after resolution: re-check, just before
/// the argv is built, that a mount still resolves outside the deny list, and
/// that an allowlist-shaped mount (read-only, destined under the sealed `$HOME`)
/// still resolves into an approved root. Fail-safe (the mount is omitted).
pub fn sealed_mount_still_ok(m: &Mount, home: &Path) -> bool {
    if sealed_mount_inside_deny(m, home) {
        return false;
    }
    // Only mounts that ORIGINATE from `sealed_home_allowlist` (read-only, destined
    // for one of its fixed `$HOME`-relative paths) must resolve into an approved
    // root; a user entry such as `/srv/data:~/data:ro` is the user's own choice.
    let from_allowlist = m.ro
        && !m.cache
        && SEALED_HOME_ALLOW
            .iter()
            .any(|rel| Path::new(&m.dest) == home.join(rel));
    if from_allowlist {
        let canon_home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        return sealed_target_ok(&resolve_lenient(Path::new(&m.host)), &canon_home);
    }
    true
}

/// Whether `host` is one of the built-in agent config dirs (`~/.claude`,
/// `~/.codex`, or the env-selected `CLAUDE_CONFIG_DIR`/`CODEX_HOME`) that a sealed
/// launch always drops — dropping those is expected, not worth a warning.
pub fn is_builtin_agent_carve(host: &str) -> bool {
    let h = Path::new(host);
    let home = std::env::var("HOME").unwrap_or_default();
    crate::account::providers().iter().any(|p| {
        crate::account::effective_config_dir(p).is_some_and(|d| Path::new(&d) == h)
            || (!home.is_empty() && Path::new(&home).join(p.default_dir) == h)
    })
}

/// The read-only `$HOME` allowlist for a sealed sandbox, resolved against the
/// real filesystem under `home`. Each entry is bound from its **canonical
/// target** (so a dotfile symlink needs no in-guest link target) at its
/// `$HOME`-relative destination; an entry that is missing, dangling, or whose
/// target resolves into a denied/unapproved location is dropped silently.
pub fn sealed_home_allowlist(home: &Path) -> Vec<Mount> {
    let Ok(canon_home) = home.canonicalize() else {
        return Vec::new();
    };
    SEALED_HOME_ALLOW
        .iter()
        .filter_map(|rel| {
            let target = home.join(rel).canonicalize().ok()?;
            (target.is_file() && sealed_target_ok(&target, &canon_home)).then(|| Mount {
                host: target.to_string_lossy().into_owned(),
                dest: home.join(rel).to_string_lossy().into_owned(),
                ro: true,
                cache: false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        std::fs::set_permissions(td.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        td
    }

    #[cfg(unix)]
    #[test]
    fn xdg_report_exposes_invalid_roots_and_overlap() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let td = private_tempdir();
        let home = td.path().join("home");
        let state = td.path().join("state");
        let data = td.path().join("data");
        std::fs::create_dir_all(state.join("bash")).unwrap();
        std::fs::create_dir_all(&data).unwrap();

        let symlink_root = td.path().join("state-link");
        symlink(&state, &symlink_root).unwrap();
        let symlink_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(symlink_root.as_os_str().to_owned()),
            _ => None,
        };
        let (mounts, omissions) =
            xdg_carveout_report_at(SandboxProfile::Hardened, &home, &symlink_env);
        assert!(mounts.iter().all(|mount| !mount.dest.ends_with("/bash")));
        assert!(omissions.iter().any(|entry| {
            entry.path == symlink_root && entry.reason == XdgOmission::SymlinkedRoot
        }));

        let non_utf8 = std::ffi::OsString::from_vec(vec![b'/', 0xff]);
        let invalid_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(non_utf8.clone()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &invalid_env);
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::InvalidRoot)
        );

        let relative_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some("relative".into()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &relative_env);
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::RelativeRoot)
        );

        let missing = td.path().join("missing-state");
        let missing_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(missing.as_os_str().to_owned()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &missing_env);
        assert!(
            omissions
                .iter()
                .any(|entry| { entry.path == missing && entry.reason == XdgOmission::MissingRoot })
        );

        let file_root = td.path().join("state-file");
        std::fs::write(&file_root, "").unwrap();
        let file_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(file_root.as_os_str().to_owned()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &file_env);
        assert!(
            omissions
                .iter()
                .any(|entry| entry.path == file_root && entry.reason == XdgOmission::NotDirectory)
        );

        let root_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(std::path::PathBuf::from("/").into_os_string()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &root_env);
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::UnresolvableParent)
        );

        let dotdot_root = state.join("..");
        let dotdot_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(dotdot_root.as_os_str().to_owned()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &dotdot_env);
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::UnresolvableRoot)
        );

        let nested = state.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let overlap_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(state.as_os_str().to_owned()),
            "XDG_DATA_HOME" => Some(nested.as_os_str().to_owned()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &overlap_env);
        assert_eq!(
            omissions
                .iter()
                .filter(|entry| entry.reason == XdgOmission::OverlappingRoots)
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_report_exposes_unsafe_leaf_omissions_and_benign_defaults() {
        use std::os::unix::fs::symlink;

        let td = private_tempdir();
        let home = td.path().join("home");
        let state = td.path().join("state");
        let data = td.path().join("data");
        let outside = td.path().join("outside");
        std::fs::create_dir_all(state.join("thegn/bash")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        symlink(state.join("thegn/bash"), state.join("bash")).unwrap();
        symlink(&outside, data.join("fish")).unwrap();
        symlink(td.path().join("missing-leaf"), data.join("atuin")).unwrap();
        std::fs::write(data.join("zoxide"), "").unwrap();
        let env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(state.as_os_str().to_owned()),
            "XDG_DATA_HOME" => Some(data.as_os_str().to_owned()),
            _ => None,
        };
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &env);
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::ProtectedState)
        );
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::LeafEscapesRoot)
        );
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::LeafNotDirectory)
        );
        assert!(
            omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::UnresolvableLeaf)
        );

        let runtime_state = td.path().join("runtime-state");
        std::fs::create_dir_all(runtime_state.join("bash/run")).unwrap();
        symlink(runtime_state.join("bash"), runtime_state.join("thegn")).unwrap();
        let runtime_env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(runtime_state.as_os_str().to_owned()),
            _ => None,
        };
        let (_, runtime_omissions) =
            xdg_carveout_report_at(SandboxProfile::Hardened, &home, &runtime_env);
        assert!(
            runtime_omissions
                .iter()
                .any(|entry| entry.reason == XdgOmission::ProtectedRuntime)
        );

        let defaults = |_key: &str| None;
        let (_, omissions) = xdg_carveout_report_at(SandboxProfile::Hardened, &home, &defaults);
        assert!(omissions.is_empty(), "missing default roots are benign");

        for reason in [
            XdgOmission::InvalidRoot,
            XdgOmission::RelativeRoot,
            XdgOmission::MissingRoot,
            XdgOmission::SymlinkedRoot,
            XdgOmission::NotDirectory,
            XdgOmission::UnresolvableRoot,
            XdgOmission::UnresolvableParent,
            XdgOmission::OverlappingRoots,
            XdgOmission::UnresolvableLeaf,
            XdgOmission::LeafEscapesRoot,
            XdgOmission::LeafNotDirectory,
            XdgOmission::ProtectedState,
            XdgOmission::ProtectedRuntime,
        ] {
            assert!(!reason.as_str().is_empty());
        }
    }

    #[test]
    fn xdg_roots_mount_only_approved_existing_leaves_and_keep_destinations() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("home");
        let state = td.path().join("state");
        let data = td.path().join("data");
        for path in [
            state.join("bash"),
            state.join("zsh"),
            state.join("thegn"),
            data.join("fish"),
            data.join("zoxide"),
            data.join("atuin"),
            data.join("other"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let lookup = |key: &str| match key {
            "XDG_STATE_HOME" => Some(state.as_os_str().to_owned()),
            "XDG_DATA_HOME" => Some(data.as_os_str().to_owned()),
            _ => None,
        };
        let mounts = default_writable_carveouts_at(SandboxProfile::Hardened, &home, &lookup);
        for path in [
            state.join("bash"),
            state.join("zsh"),
            data.join("fish"),
            data.join("zoxide"),
            data.join("atuin"),
        ] {
            assert!(
                mounts
                    .iter()
                    .any(|mount| mount.dest == path.to_string_lossy().as_ref()),
                "missing {path:?}"
            );
        }
        assert!(
            mounts
                .iter()
                .all(|mount| mount.dest != state.to_string_lossy().as_ref()
                    && mount.dest != data.to_string_lossy().as_ref()
                    && mount.dest != state.join("thegn").to_string_lossy().as_ref()
                    && mount.dest != data.join("thegn").to_string_lossy().as_ref())
        );
        let sealed = default_writable_carveouts_at(SandboxProfile::Sealed, &home, &lookup);
        assert!(
            sealed
                .iter()
                .all(|mount| mount.dest != data.join("atuin").to_string_lossy().as_ref())
        );

        let relative = |key: &str| match key {
            "XDG_STATE_HOME" => Some("relative".into()),
            "XDG_DATA_HOME" => Some(data.as_os_str().to_owned()),
            _ => None,
        };
        let invalid = default_writable_carveouts_at(SandboxProfile::Hardened, &home, &relative);
        assert!(invalid.iter().all(|mount| !mount.dest.ends_with("/bash")));

        let default_home = td.path().join("default-home");
        for path in [
            default_home.join(".local/state/bash"),
            default_home.join(".local/share/fish"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let no_overrides = |_key: &str| None;
        let defaults =
            default_writable_carveouts_at(SandboxProfile::Hardened, &default_home, &no_overrides);
        assert!(defaults.iter().any(|mount| {
            mount.dest
                == default_home
                    .join(".local/state/bash")
                    .to_string_lossy()
                    .as_ref()
        }));
        assert!(defaults.iter().any(|mount| {
            mount.dest
                == default_home
                    .join(".local/share/fish")
                    .to_string_lossy()
                    .as_ref()
        }));

        std::fs::create_dir_all(state.join("nested")).unwrap();
        let overlap = |key: &str| match key {
            "XDG_STATE_HOME" => Some(state.as_os_str().to_owned()),
            "XDG_DATA_HOME" => Some(state.join("nested").into_os_string()),
            _ => None,
        };
        let overlapping = default_writable_carveouts_at(SandboxProfile::Hardened, &home, &overlap);
        assert!(
            overlapping.iter().all(|mount| {
                !mount.dest.ends_with("/bash") && !mount.dest.ends_with("/zoxide")
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_state_root_leaf_symlink_is_omitted() {
        use std::os::unix::fs::symlink;
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("home");
        let real = td.path().join("state-real");
        let alias = td.path().join("state-alias");
        std::fs::create_dir_all(real.join("bash")).unwrap();
        symlink(&real, &alias).unwrap();
        let env = |key: &str| match key {
            "XDG_STATE_HOME" => Some(alias.as_os_str().to_owned()),
            _ => None,
        };
        let mounts = default_writable_carveouts_at(SandboxProfile::Hardened, &home, &env);
        assert!(mounts.iter().all(|mount| !mount.dest.ends_with("/bash")));

        let real_home = td.path().join("real-home");
        let namespace_home = td.path().join("home-alias");
        std::fs::create_dir_all(real_home.join(".local/state/bash")).unwrap();
        symlink(&real_home, &namespace_home).unwrap();
        let no_overrides = |_key: &str| None;
        let alias_mounts =
            default_writable_carveouts_at(SandboxProfile::Hardened, &namespace_home, &no_overrides);
        let alias_dest = namespace_home.join(".local/state/bash");
        let alias_source = real_home.join(".local/state/bash");
        assert!(alias_mounts.iter().any(|mount| {
            mount.dest == alias_dest.to_string_lossy().as_ref()
                && mount.host == alias_source.to_string_lossy().as_ref()
        }));
    }

    #[test]
    fn systemd_paths_are_refused_unless_plain_absolute_and_normalised() {
        for ok in ["/", "/wt/feat", "/wt/feat/", "/nix/store/x-a.b_c+d"] {
            assert_eq!(systemd_path_refusal(ok), None, "{ok}");
        }
        for bad in [
            "", "rel/path", "-/wt", "+/wt", "/a:b", "/a b", "/a\tb", "/a\nb", "/a\\b", "/a\"b",
            "/a'b", "/a%hb", "/a//b", "/a/./b", "/a/../b", "/a/..",
        ] {
            assert!(systemd_path_refusal(bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn systemd_bind_property_encodes_plain_pairs_and_refuses_others() {
        assert_eq!(
            systemd_bind_property("BindPaths", "/a", "/b").as_deref(),
            Ok("BindPaths=/a:/b")
        );
        let err = systemd_bind_property("BindReadOnlyPaths", "/a:evil", "/b").unwrap_err();
        assert!(err.contains("/a:evil"), "{err}");
        assert!(systemd_bind_property("BindPaths", "/a", "/b c").is_err());
        assert_eq!(
            systemd_path_property("ReadWritePaths", "/w").as_deref(),
            Ok("ReadWritePaths=/w")
        );
        assert!(systemd_path_property("ReadWritePaths", "-/w").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolver_bind_dir_follows_the_symlink_chain_to_the_run_dir() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let run = root.join("run");
        let resolve = run.join("systemd").join("resolve");
        std::fs::create_dir_all(&resolve).unwrap();
        std::fs::write(resolve.join("stub-resolv.conf"), "nameserver 127.0.0.53\n").unwrap();
        let etc = root.join("etc");
        std::fs::create_dir_all(etc.join("static")).unwrap();
        std::os::unix::fs::symlink(
            resolve.join("stub-resolv.conf"),
            etc.join("static").join("resolv.conf"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            etc.join("static").join("resolv.conf"),
            etc.join("resolv.conf"),
        )
        .unwrap();
        let etc_s = etc.to_string_lossy().into_owned();
        assert_eq!(
            resolver_bind_dir(&etc.join("resolv.conf"), &run, &[etc_s.as_str()]),
            Some(resolve.clone())
        );
        // Already covered by a bound substrate path: nothing extra to bind.
        let resolve_s = resolve.to_string_lossy().into_owned();
        assert_eq!(
            resolver_bind_dir(&etc.join("resolv.conf"), &run, &[resolve_s.as_str()]),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolver_bind_dir_refuses_targets_outside_the_runtime_root() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        // A plain file under /etc (the non-resolved case): nothing to bind.
        let plain = root.join("resolv.conf");
        std::fs::write(&plain, "nameserver 1.1.1.1\n").unwrap();
        assert_eq!(resolver_bind_dir(&plain, &run, &[]), None);
        // A target directly in the runtime root would bind all of /run.
        let direct = run.join("resolv.conf");
        std::fs::write(&direct, "x").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&direct, &link).unwrap();
        assert_eq!(resolver_bind_dir(&link, &run, &[]), None);
        // Dangling link: unresolvable, so refused.
        let dangling = root.join("dangling");
        std::os::unix::fs::symlink(run.join("gone").join("x"), &dangling).unwrap();
        assert_eq!(resolver_bind_dir(&dangling, &run, &[]), None);
        // Even though this is below /run, exposing a user session directory
        // would reveal sockets such as the session bus and agent credentials.
        let user_dir = run.join("user").join("1000");
        std::fs::create_dir_all(&user_dir).unwrap();
        let user_target = user_dir.join("resolv.conf");
        std::fs::write(&user_target, "x").unwrap();
        let user_link = root.join("user-link");
        std::os::unix::fs::symlink(&user_target, &user_link).unwrap();
        assert_eq!(resolver_bind_dir(&user_link, &run, &[]), None);
    }

    #[test]
    fn carveouts_are_rw_existing_paths_under_home() {
        // Every carve-out is a read-write, non-cache path (dir or file) that
        // exists — bwrap needs the mountpoint present to overmount it.
        for m in default_writable_carveouts(SandboxProfile::Hardened) {
            assert!(!m.ro, "carve-out must be writable: {}", m.host);
            let p = std::path::Path::new(&m.host);
            assert!(
                p.is_dir() || p.is_file(),
                "carve-out must be an existing dir or file: {}",
                m.host
            );
        }
    }

    #[test]
    fn keychain_carved_for_hardened_not_sealed() {
        let home = std::env::var("HOME").unwrap_or_default();
        // Only meaningful when ~/.keychain actually exists on this host.
        if home.is_empty() || !std::path::Path::new(&home).join(".keychain").exists() {
            return;
        }
        let has_keychain = |profile| {
            default_writable_carveouts(profile)
                .iter()
                .any(|m| m.host.ends_with("/.keychain"))
        };
        assert!(
            has_keychain(SandboxProfile::Hardened),
            "hardened profile should carve ~/.keychain writable"
        );
        assert!(
            !has_keychain(SandboxProfile::Sealed),
            "sealed agent profile must NOT carve ~/.keychain (persistence vector)"
        );
        assert!(
            !has_keychain(SandboxProfile::SealedTunnel),
            "sealed-tunnel profile must NOT carve ~/.keychain"
        );
    }

    #[test]
    fn nix_client_cache_carved_writable() {
        // ~/.cache/nix (flake tarball + eval caches) must be writable so an
        // in-sandbox nix-direnv cache-miss re-eval can fetch flake inputs instead
        // of dying "Read-only file system". Only meaningful when it exists.
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() || !std::path::Path::new(&home).join(".cache/nix").is_dir() {
            return;
        }
        assert!(
            auto_cache_mounts()
                .iter()
                .any(|m| m.host.ends_with("/.cache/nix") && !m.ro),
            "~/.cache/nix must be carved writable for in-sandbox flake re-eval"
        );
    }

    #[test]
    fn claude_profiles_carved_for_hardened_not_sealed() {
        // HOME-swapping profile wrappers (claude-profile / claude-multiplex) put
        // each profile's config+session under ~/.claude-profiles/<name>; it must
        // be writable so the agent's `session-env` mkdir doesn't EROFS — but NOT
        // for the sealed agent. Only meaningful when the dir exists on this host.
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty()
            || !std::path::Path::new(&home)
                .join(".claude-profiles")
                .is_dir()
        {
            return;
        }
        let has = |profile| {
            default_writable_carveouts(profile)
                .iter()
                .any(|m| m.host.ends_with("/.claude-profiles") && !m.ro)
        };
        assert!(
            has(SandboxProfile::Hardened),
            "hardened profile should carve ~/.claude-profiles writable"
        );
        assert!(
            !has(SandboxProfile::Sealed),
            "sealed agent profile must NOT carve ~/.claude-profiles"
        );
        assert!(
            !has(SandboxProfile::SealedTunnel),
            "sealed-tunnel profile must NOT carve ~/.claude-profiles"
        );
    }

    #[test]
    fn claude_projects_carved_for_hardened_not_sealed() {
        // ~/.claude/projects is the SHARED base each HOME-swapped profile symlinks
        // its own projects/ into (cross-profile agent memory + transcripts). It
        // must be writable so those writes don't EROFS under the read-only $HOME —
        // but NOT for the sealed agent (which must stay out of the shared store).
        // Only meaningful when the dir exists on this host.
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty()
            || !std::path::Path::new(&home)
                .join(".claude/projects")
                .is_dir()
        {
            return;
        }
        let has = |profile| {
            default_writable_carveouts(profile)
                .iter()
                .any(|m| m.host.ends_with("/.claude/projects") && !m.ro)
        };
        assert!(
            has(SandboxProfile::Hardened),
            "hardened profile should carve ~/.claude/projects writable"
        );
        assert!(
            !has(SandboxProfile::Sealed),
            "sealed agent profile must NOT carve ~/.claude/projects"
        );
        assert!(
            !has(SandboxProfile::SealedTunnel),
            "sealed-tunnel profile must NOT carve ~/.claude/projects"
        );
    }

    #[test]
    fn history_files_carved_for_all_ro_profiles() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let hist = std::path::Path::new(&home).join(".zsh_history");
        // Only assert when the file exists (carve-outs skip absent paths).
        if !hist.is_file() {
            return;
        }
        for profile in [
            SandboxProfile::Hardened,
            SandboxProfile::Sealed,
            SandboxProfile::SealedTunnel,
        ] {
            assert!(
                default_writable_carveouts(profile)
                    .iter()
                    .any(|m| m.host.ends_with("/.zsh_history") && !m.ro),
                "history file must be carved writable for every read-only-root profile"
            );
        }
    }

    #[test]
    fn agent_config_dir_carved_for_hardened_not_sealed() {
        // The launched coding-agent's config dir (`CLAUDE_CONFIG_DIR` here) must
        // be writable so its `SessionStart` hook can create `session-env` under a
        // read-only $HOME — but NOT for the sealed agent profile. A sibling
        // non-existent dir must never be carved (existence check refuses to
        // fabricate one). Mutates only `CLAUDE_CONFIG_DIR` (targeted, restored) —
        // no `HOME` churn, matching `bundle.rs`'s unmanaged-agent test.
        let base = std::env::temp_dir().join(format!("tg-agentcfg-{}", crate::util::now()));
        let present = base.join("present");
        let absent = base.join("absent"); // deliberately not created
        std::fs::create_dir_all(&present).unwrap();
        let present_s = present.to_string_lossy().into_owned();
        let absent_s = absent.to_string_lossy().into_owned();
        let prev = std::env::var("CLAUDE_CONFIG_DIR").ok(); // best-effort: test: the saved env value may legitimately be unset

        // SAFETY: single-threaded test; `CLAUDE_CONFIG_DIR` is read only by
        // `account::effective_config_dir`, reached via `default_writable_carveouts`.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &present_s) };
        let hardened = default_writable_carveouts(SandboxProfile::Hardened);
        let sealed = default_writable_carveouts(SandboxProfile::Sealed);
        let sealed_tunnel = default_writable_carveouts(SandboxProfile::SealedTunnel);
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &absent_s) };
        let hardened_absent = default_writable_carveouts(SandboxProfile::Hardened);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
        }
        std::fs::remove_dir_all(&base).ok(); // best-effort: test cleanup: scratch removal must never fail the test

        assert!(
            hardened.iter().any(|m| m.host == present_s && !m.ro),
            "hardened profile must carve the agent config dir writable"
        );
        assert!(
            !sealed.iter().any(|m| m.host == present_s),
            "sealed agent profile must NOT carve the host config dir writable"
        );
        assert!(
            !sealed_tunnel.iter().any(|m| m.host == present_s),
            "sealed-tunnel profile must NOT carve the host config dir writable"
        );
        assert!(
            !hardened_absent.iter().any(|m| m.host == absent_s),
            "a non-existent config dir must never be carved"
        );
    }

    #[test]
    fn ro_home_flag_controls_home_writability() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() || !std::path::Path::new(&home).exists() {
            return;
        }
        let ro = host_toolchain_mounts_ro_home(true);
        let rw = host_toolchain_mounts_ro_home(false);
        let home_mount = |ms: &[Mount]| ms.iter().find(|m| m.host == home).map(|m| m.ro);
        assert_eq!(
            home_mount(&ro),
            Some(true),
            "read-only profile must bind $HOME read-only"
        );
        assert_eq!(
            home_mount(&rw),
            Some(false),
            "open profile must bind $HOME read-write"
        );
    }

    #[test]
    fn keep_cfg_mount_rw_dir_overmounts_ro_parent() {
        let home = Mount {
            host: "/home/u".into(),
            dest: "/home/u".into(),
            ro: true,
            cache: false,
        };
        let existing = std::slice::from_ref(&home);
        // An entry at the exact same path as an existing mount → dropped.
        let tmp = Mount {
            host: "/home/u".into(),
            dest: "/home/u".into(),
            ro: false,
            cache: false,
        };
        assert!(!keep_cfg_mount(existing, &tmp));
        // A path not covered by any existing mount → kept.
        let elsewhere = Mount {
            host: "/opt/thing".into(),
            dest: "/opt/thing".into(),
            ro: false,
            cache: false,
        };
        assert!(keep_cfg_mount(existing, &elsewhere));
    }

    #[test]
    fn keep_cfg_mount_rw_file_overmounts_ro_parent() {
        // A read-write *existing file* strictly under a read-only parent is kept
        // (bwrap overmounts the existing inode). Use a real file under a
        // synthetic ro parent so the test doesn't couple to $HOME.
        let hostname = "/etc/hostname";
        if !std::path::Path::new(hostname).is_file() {
            return; // portability guard
        }
        let etc = Mount {
            host: "/etc".into(),
            dest: "/etc".into(),
            ro: true,
            cache: false,
        };
        let existing = std::slice::from_ref(&etc);
        let hist = Mount {
            host: hostname.into(),
            dest: hostname.into(),
            ro: false,
            cache: false,
        };
        assert!(
            keep_cfg_mount(existing, &hist),
            "a rw existing file under a ro parent must overmount it"
        );
        // A non-existent file mountpoint under the ro parent → dropped.
        let ghost = Mount {
            host: "/etc/tg-nonexistent-xyz".into(),
            dest: "/etc/tg-nonexistent-xyz".into(),
            ro: false,
            cache: false,
        };
        assert!(!keep_cfg_mount(existing, &ghost));
    }
}
