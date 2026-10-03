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
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }
    // Personal scratch + the XDG state/data dirs where shell history, zoxide,
    // and atuin keep their databases, plus the legacy `$HOME`-root history files
    // many shells still write. Deliberately narrow: source trees, ~/.ssh,
    // ~/.config, and the rest of $HOME stay read-only.
    let mut rels: Vec<&str> = vec![
        "tmp",
        ".local/state",
        ".local/share",
        ".zsh_history",
        ".bash_history",
    ];
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
            let p = std::path::Path::new(&home).join(rel);
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
    mounts
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

#[cfg(test)]
mod tests {
    use super::*;

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

/// Backends that can hide `$HOME` for a sealed profile.
fn family_can_hide_home(family: BackendFamily) -> bool {
    matches!(
        family,
        BackendFamily::Oci | BackendFamily::Bwrap | BackendFamily::Systemd
    )
}

/// What a *planned* launch under `profile` on `backend` shows of `$HOME` (doctor,
/// before any spec exists). Mirrors [`home_view`], which answers for a real spec.
pub fn planned_home_view(profile: SandboxProfile, backend: Backend) -> HomeView {
    let family = backend.profile().family;
    if profile.hides_home() && family_can_hide_home(family) {
        return HomeView::Hidden;
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
    match family {
        BackendFamily::Systemd => {
            if spec.seal_home.is_some() {
                HomeView::Hidden
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
            // The tightest mount that is an ancestor-or-equal of `$HOME`.
            let covering = spec
                .mounts
                .iter()
                .filter(|m| home.starts_with(&m.dest))
                .max_by_key(|m| m.dest.len());
            match covering {
                Some(m) if m.ro => HomeView::ReadOnly,
                Some(_) => HomeView::Writable,
                None => HomeView::Hidden,
            }
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
];

/// Absolute roots that are secret by convention wherever they are linked from.
const SEALED_ABS_DENY: &[&str] = &["/run/agenix", "/run/secrets", "/run/user", "/root"];

/// Absolute roots a resolved allowlist target may live under besides `$HOME`
/// itself (home-manager and similar link dotfiles into the Nix store).
const SEALED_ABS_APPROVED: &[&str] = &["/nix/store", "/usr", "/etc"];

/// Dotfiles/config a sealed pane keeps read-only so the user's real shell, git
/// identity and prompt work. Reviewed list — adding a credential-bearing path
/// here is a security change. Extend per-user via `[sandbox] mounts`.
const SEALED_HOME_ALLOW: &[&str] = &[
    ".gitconfig",
    ".config/git",
    ".zshenv",
    ".zshrc",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".config/zsh",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".inputrc",
    ".config/starship.toml",
    ".terminfo",
];

/// Whether the resolved `target` may be exposed in a sealed `$HOME`: under an
/// approved non-secret root (the home itself minus the deny list, or the Nix
/// store / `/usr` / `/etc`), not under a denied path, and not an *ancestor* of
/// one (a link to `~/.config` would drag `~/.config/gh` in with it).
fn sealed_target_ok(target: &Path, home: &Path) -> bool {
    let denied: Vec<PathBuf> = SEALED_HOME_DENY
        .iter()
        .map(|d| home.join(d))
        .chain(SEALED_ABS_DENY.iter().map(PathBuf::from))
        .collect();
    if denied
        .iter()
        .any(|d| target.starts_with(d) || d.starts_with(target))
    {
        return false;
    }
    target.starts_with(home) || SEALED_ABS_APPROVED.iter().any(|r| target.starts_with(r))
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
            sealed_target_ok(&target, &canon_home).then(|| Mount {
                host: target.to_string_lossy().into_owned(),
                dest: home.join(rel).to_string_lossy().into_owned(),
                ro: true,
                cache: false,
            })
        })
        .collect()
}
