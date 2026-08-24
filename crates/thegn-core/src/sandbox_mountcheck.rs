//! Verify, from **inside** the container, that the worktree bind actually
//! delivered files — the one sandbox failure the runtime cannot report.
//!
//! thegn bind-mounts a worktree at its own absolute path (`-v <wt>:<wt>`);
//! `sandbox.rs`'s module doc explains why that path-preservation is load-bearing.
//! On a host whose containers run inside a **VM** — podman machine, colima,
//! Docker Desktop on macOS; WSL2 on Windows — `-v` is resolved *inside the VM*,
//! which only sees the host directories it was told to share. When the worktree
//! is outside that set the runtime does not refuse: it creates an **empty
//! directory** and starts the container.
//!
//! Every check thegn had then agreed:
//!   * [`crate::sandbox`]'s `container_status` builds its `required` set from
//!     `spec.mounts[].host` — the strings thegn asked for — and compares them to
//!     `.Mounts[].Source`, which the runtime echoes back unchanged. It compares
//!     a request to a copy of that request, so it always matches.
//!   * `sandbox_preflight`'s probe ran `/bin/sh -lc true` with
//!     `--workdir <worktree>`. The empty directory exists, so `cd` and `true`
//!     both succeed.
//!
//! Neither asked whether the *files* arrived. The pane opened on an empty
//! worktree while thegn reported real containment — a silent failure, unlike the
//! loud `Exec format error` class this module's sibling fixes.
//!
//! ## The safety property
//!
//! **Only ever assert a path we just observed on the host.** [`mount_sentinels`]
//! takes `host_exists` as an argument and returns nothing when it cannot prove
//! anything, in which case [`preflight_probe_body`] emits the literal `true` this
//! probe used before — byte-identical behaviour. A sentinel can therefore only
//! fail when the host has the file and the container does not, which has exactly
//! one cause. That is what makes this un-regressable rather than a new way for a
//! working sandbox to be refused.
//!
//! Everything here is pure (paths and OS come in as values) so the whole matrix
//! is unit-tested from one machine, in the shape `sandbox_dormant::start_argv`
//! and `sandbox_support::remedy_for` already use.

use crate::config::FileAccess;
use crate::sandbox::{Backend, SandboxSpec};
use crate::sandbox_backend::HostOs;

/// Printed to stderr by the in-container probe for the one failure a runtime
/// cannot report. Matched by [`parse_missing_sentinel`].
///
/// A stderr marker rather than an exit code: podman and docker both propagate
/// the command's status, but Apple `container` and `wsl.exe --` are not worth
/// betting an error class on, and 125/126/127 are already reserved by the
/// engines. Scanning for a prefix is engine-agnostic and survives the profile
/// noise a `-l` login shell writes to stderr.
pub const MOUNT_MISSING_MARKER: &str = "thegn-mount-missing:";

/// Paths that MUST exist inside the container if the worktree binds worked.
///
/// `host_exists` is injected rather than probed here so the choice stays pure —
/// and so the safety property above is enforced by construction: a path is only
/// asserted after the host was seen to have it.
///
/// Empty (⇒ the probe stays `true`) whenever we cannot prove a failure:
///   * [`FileAccess::None`] — nothing was mounted, so asserting anything would
///     be a false positive;
///   * a non-local placement — the files live on a machine whose filesystem we
///     never inspected, and we do not invent facts about it;
///   * a compose spec with a service — the pane enters via
///     `compose exec <service>`, which is a different container than the probe
///     targets (a pre-existing mismatch this module deliberately does not widen);
///   * any path the host does not have.
pub fn mount_sentinels(spec: &SandboxSpec, host_exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    if spec.file_access == FileAccess::None || !spec.placement.is_local() {
        return Vec::new();
    }
    if spec
        .compose_spec()
        .is_some_and(|c: crate::sandbox_compose::ComposeSpec| c.has_service())
    {
        return Vec::new();
    }

    let mut out = Vec::new();
    let wt = spec.worktree.to_string_lossy();

    // `.git` is the sentinel because it is the one thing EVERY worktree has — a
    // file for a linked worktree (holding the absolute `gitdir:` pointer), a
    // directory for the main checkout — and `-e` covers both. It is also
    // precisely the artifact whose absence breaks the invariant this protects.
    //
    // Rejected alternatives: `git -C <wt> rev-parse` needs git in the image
    // (never guaranteed; a sealed image is minimal) and conflates "mount broken"
    // with "no git installed"; "is the directory non-empty" false-fails a
    // legitimately empty tree.
    let dot_git = format!("{wt}/.git");
    if host_exists(&dot_git) {
        out.push(dot_git);
    }

    // The repo's git-common dir is a separate bind, and it can live on a
    // different volume than the worktree (main repo on an external disk, linked
    // worktree under $HOME) — so it can fail independently. Identify it by SHAPE
    // rather than re-deriving it: a non-worktree mount whose host side holds a
    // `HEAD`. `host_exists` filters the toolchain and cache mounts out for free.
    // Use `dest` for the in-container path and `host` for the host check, since
    // a user `[sandbox] mounts` entry can remap the two.
    for m in &spec.mounts {
        if m.dest == spec.worktree.to_string_lossy() {
            continue;
        }
        if host_exists(&format!("{}/HEAD", m.host)) {
            out.push(format!("{}/HEAD", m.dest));
            break; // one is enough; this is a probe, not an audit
        }
    }
    out
}

/// The `/bin/sh -lc` body for the preflight probe.
///
/// `"true"` when there is nothing provable — byte-identical to the probe this
/// replaced, so a spec we cannot reason about behaves exactly as before.
pub fn preflight_probe_body(sentinels: &[String]) -> String {
    if sentinels.is_empty() {
        return "true".to_string();
    }
    let list = sentinels
        .iter()
        .map(|p| crate::util::sh_quote(p))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "for p in {list}; do [ -e \"$p\" ] || \
         {{ printf '{MOUNT_MISSING_MARKER}%s\\n' \"$p\" >&2; exit 97; }}; done"
    )
}

/// The missing path out of the probe's stderr, ignoring login-shell noise.
pub fn parse_missing_sentinel(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .find_map(|l| l.trim().strip_prefix(MOUNT_MISSING_MARKER))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The bind source a runtime refused to mount, out of a failed `run`'s stderr.
///
/// The complement to [`parse_missing_sentinel`]: podman and docker **do** refuse
/// an unshared bind rather than silently mounting an empty directory (verified
/// against podman 5.8.6 on macOS 26 — `run` exits 125 and no container is
/// created), so on those runtimes the failure arrives here, at create, and the
/// in-container probe never gets to run. What was missing was not detection but
/// the *message*: `ensure` discarded the runtime's stderr and reported a generic
/// "could not start podman container '<name>'", so the one line that named the
/// cause — and the remedy it implies — never reached the user.
///
/// Only signatures observed against a real runtime are matched. `wsl.exe` is
/// deliberately absent rather than guessed: an unrecognized stderr falls through
/// to the existing generic error, which is exactly today's behaviour, so a wrong
/// guess cannot mask a different failure.
pub fn parse_unshared_bind(stderr: &str) -> Option<&str> {
    stderr.lines().find_map(|line| {
        let l = line.trim();
        // podman / crun: `Error: statfs /opt/x: no such file or directory`
        let after = l
            .strip_prefix("Error: statfs ")
            .or_else(|| l.strip_prefix("statfs "))
            // docker: `... bind source path does not exist: /opt/x`
            .or_else(|| {
                l.rsplit_once("bind source path does not exist: ")
                    .map(|(_, p)| p)
            })
            // Apple `container`: `Error: path '/opt/x' does not exist` (exit 1,
            // no container created — verified on macOS 26).
            .or_else(|| {
                l.strip_prefix("Error: path '")
                    .and_then(|r| r.split_once('\'').map(|(p, _)| p))
            })?;
        // The podman form carries a trailing `: <errno text>`; the docker and
        // Apple forms do not. Cut at the first `: ` so all three yield the bare
        // path. (Apple's is already delimited by the closing quote.)
        let path = after.split_once(": ").map_or(after, |(p, _)| p).trim();
        path.starts_with('/').then_some(path)
    })
}

/// What the caller knows about a verified mount failure, for [`mount_failure`].
pub struct MountProbe<'a> {
    pub backend: Backend,
    pub os: HostOs,
    pub file_access: FileAccess,
    /// The worktree path, already canonicalized by the caller — on macOS `/tmp`
    /// and `/var` are symlinks into `/private`, and the VM shares `/private`, so
    /// an uncanonicalized path yields the wrong share root.
    pub worktree: &'a str,
    pub missing: &'a str,
}

/// A failure split so truncation cannot cost the diagnosis.
///
/// The warning lands in `model.status`, which is width-fitted, so `headline`
/// names the failure and the missing path and `remedy` trails it. The full text
/// still reaches the log via `msg::warn`.
pub struct MountFailure {
    pub headline: String,
    pub remedy: String,
}

impl MountFailure {
    pub fn one_line(&self) -> String {
        format!("{} — {}", self.headline, self.remedy)
    }
}

/// The first path component — the granularity every VM shares at.
fn share_root(path: &str) -> Option<&str> {
    let rest = path.strip_prefix('/')?;
    let end = rest.find('/').unwrap_or(rest.len());
    (end > 0).then(|| &path[..=end])
}

/// podman machine's fixed default shares. Changing them requires recreating the
/// machine — `podman machine set` has no `--volume`.
const PODMAN_DEFAULT_SHARES: [&str; 3] = ["/Users", "/private", "/var/folders"];

/// A runtime- and OS-specific remedy for a verified mount failure.
///
/// `have` answers "is this binary on PATH?", injected exactly as
/// `sandbox_dormant::start_argv` does for colima, so the matrix stays pure.
pub fn mount_failure(p: &MountProbe<'_>, have: &dyn Fn(&str) -> bool) -> MountFailure {
    // True whether the failure surfaced at create (the runtime refused the bind)
    // or at exec (the bind produced an empty directory) — the two paths differ in
    // where they are caught, not in what is wrong.
    let headline = format!(
        "{} exists on this host but {} could not see it",
        p.missing,
        p.backend.label()
    );
    // The root to share is the MISSING path's, not the worktree's: a bind can
    // fail for a mount that lives somewhere else entirely (a main repo on an
    // external volume with its linked worktree under $HOME), and naming the
    // worktree's root would send the user to share a directory that is already
    // working. For the in-container probe the two coincide, since every sentinel
    // is derived from a mount.
    let root = share_root(p.missing)
        .or_else(|| share_root(p.worktree))
        .unwrap_or("/");
    let already_shared = PODMAN_DEFAULT_SHARES.contains(&root);

    let remedy = match (p.os, p.backend) {
        // A root bind inside a VM-backed runtime binds the VM's root, not the
        // Mac's — a different bug with a different fix, so diagnose it as itself
        // rather than sending someone to widen a share that is irrelevant.
        (HostOs::MacOs, _) if matches!(p.file_access, FileAccess::All | FileAccess::Host) => {
            "on macOS the container runs in a Linux VM, so a whole-root bind binds the VM's \
             root, not your Mac's. Use `[sandbox] file_access = \"worktree\"`."
                .to_string()
        }
        (HostOs::MacOs, Backend::Podman | Backend::PodmanRootful) if !already_shared => format!(
            "podman runs Linux containers in a VM that only shares {}. `podman machine set` \
             has no --volume, so widening it means recreating the machine: `podman machine rm` \
             then `podman machine init -v {root}:{root}`. Or move the worktree under /Users.",
            PODMAN_DEFAULT_SHARES.join(", ")
        ),
        // Already inside the share set ⇒ a different cause. Do NOT tell someone
        // to re-add a directory the VM already shares.
        (HostOs::MacOs, Backend::Podman | Backend::PodmanRootful) => format!(
            "{root} is already shared by podman's VM, so the share is not the problem — check \
             the VM itself with `podman machine ssh ls {}`.",
            p.worktree
        ),
        (HostOs::MacOs, Backend::Docker | Backend::Smol) if have("colima") => format!(
            "colima shares only $HOME by default. Restart it with {root} shared: \
             `colima stop && colima start -V {root}:w`."
        ),
        (HostOs::MacOs, Backend::Docker | Backend::Smol) => format!(
            "Docker Desktop shares /Users, /Volumes, /private and /tmp. Add {root} under \
             Settings → Resources → File Sharing, then restart Docker."
        ),
        // Apple's `container` has NO fixed share set — measured on macOS 26, it
        // binds an arbitrary host path (`/opt/homebrew` arrives complete), and
        // the only observed refusal is for a path that is genuinely absent:
        // `Error: path '<p>' does not exist`, exit 1, no container. So the
        // share-widening advice every other macOS runtime needs is not merely
        // unnecessary here, it is a dead end — an earlier draft of this arm told
        // the user to move the worktree under /Users, which fixes nothing.
        (HostOs::MacOs, Backend::Apple) => format!(
            "Apple's `container` binds host paths directly rather than through a fixed set of \
             shares, so this is not a sharing setting — check that {} still exists and is \
             readable by your user.",
            p.missing
        ),
        (HostOs::Windows, _) => format!(
            "Windows runs Linux containers in a VM. Share {root} under Docker Desktop → \
             Settings → Resources, or use a WSL2 path."
        ),
        // On Linux host and guest share a kernel, so a missing bind is a local
        // problem — name the ones that actually happen.
        (HostOs::Linux, _) => format!(
            "the bind source vanished, is unreadable by the runtime's user (rootless UID \
             mapping), or an SELinux relabel did not take — try adding `:z` to the mount for \
             {root}."
        ),
        // macOS with a non-OCI backend can't actually reach here (bwrap/jobobject
        // are OS-gated off), but the match must be total.
        _ => format!("the runtime could not see {} on this host.", p.missing),
    };
    MountFailure { headline, remedy }
}

#[cfg(test)]
#[path = "sandbox_mountcheck_tests.rs"]
mod tests;
