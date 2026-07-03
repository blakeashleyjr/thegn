//! Host-toolchain, cache, and writable-carve-out mount builders for the local
//! (bwrap/systemd/OCI) sandbox backends.
//!
//! Extracted from `sandbox.rs` (which is at its god-file ratchet ceiling). These
//! functions turn "reuse the host toolchain" into a concrete list of
//! path-preserving [`Mount`]s: the read-only substrate ($HOME dotfiles,
//! `/nix/store`, FHS dirs, identity files), the read-write build caches, and —
//! under a read-only `$HOME` (the default hardened profile) — a narrow set of
//! read-write carve-outs so shell/tool state keeps working.

use crate::sandbox::Mount;

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
    if !home.is_empty() && exists(&home) {
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
/// existing directories are returned (bwrap needs the mountpoint to exist).
///
/// `/tmp` is intentionally absent — it is already writable on every backend
/// (bwrap `--tmpfs /tmp`, OCI `--tmpfs /tmp`, systemd `PrivateTmp=yes`). Users
/// extend this via `[sandbox] mounts` (e.g. `~/.gnupg`, a custom history dir);
/// the resolve-time covered-check ([`keep_cfg_mount`]) lets a read-write
/// directory overmount the read-only `$HOME`.
pub fn default_writable_carveouts() -> Vec<Mount> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }
    // Personal scratch + the XDG state/data dirs where shell history, zoxide,
    // and atuin keep their databases. Deliberately narrow: source trees,
    // ~/.ssh, ~/.config, and the rest of $HOME stay read-only.
    let rels = ["tmp", ".local/state", ".local/share"];
    rels.iter()
        .filter_map(|rel| {
            let p = std::path::Path::new(&home).join(rel);
            p.is_dir().then(|| {
                let s = p.to_string_lossy().into_owned();
                Mount {
                    host: s.clone(),
                    dest: s,
                    ro: false,
                    cache: false,
                }
            })
        })
        .collect()
}

/// Decide whether a mount `m` should be added given the mounts already
/// assembled in `existing`. Returns `true` to keep it.
///
/// The tightest already-mounted parent wins:
/// - not covered by any parent → keep;
/// - exact duplicate of an existing mount → drop;
/// - a read-write **directory** strictly under a **read-only** parent → keep:
///   it overmounts the parent read-write (e.g. `~/tmp` or `~/.gnupg` under a
///   read-only `$HOME`). This is what makes the writable carve-outs work;
/// - anything else already covered (a read-only entry, a read-write path under
///   a read-write parent, or a **file** mountpoint inside a bound dir which
///   bwrap cannot create) → drop.
pub fn keep_cfg_mount(existing: &[Mount], m: &Mount) -> bool {
    let mpath = std::path::Path::new(&m.host);
    let parent = existing
        .iter()
        .filter(|e| mpath.starts_with(&e.host))
        .max_by_key(|e| e.host.len());
    match parent {
        None => true,
        Some(p) if p.host == m.host => false,
        Some(p) => !m.ro && p.ro && mpath.is_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carveouts_are_rw_dirs_under_home() {
        // Every carve-out is a read-write, non-cache directory that exists.
        for m in default_writable_carveouts() {
            assert!(!m.ro, "carve-out must be writable: {}", m.host);
            assert!(
                std::path::Path::new(&m.host).is_dir(),
                "carve-out must be an existing dir: {}",
                m.host
            );
        }
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
}
