//! Small serde `default = "…"` helper functions shared across the `[config]`
//! structs. They live here (rather than inline in `config.rs`) to keep that
//! god-file shrinking; each is referenced by name from a `#[serde(default =
//! "…")]` attribute, which resolves through the `use` in `config.rs`.

/// Default for `bool` fields that should be on unless explicitly disabled.
pub(crate) fn default_true() -> bool {
    true
}

/// Default `kind` for a git custom-command prompt (`[[git_commands.prompts]]`).
pub(crate) fn default_prompt_kind() -> String {
    "input".into()
}

/// Default `context` for a git custom command (`[[git_commands]]`): every view.
pub(crate) fn default_git_context() -> String {
    "global".into()
}

/// Default `[sandbox] backend_chain` probe order. One chain serves every
/// platform, because each OS-native entry probes Absent off its own OS.
///
/// `"appcontainer"` is the Windows-native peer of `bwrap`: a token boundary
/// (own container SID, deny-by-default filesystem/registry, capability-gated
/// network) with no VM and no path translation. It sits BELOW the OCI entries
/// because those are a stronger class — a real kernel namespace boundary —
/// and above `host`, which is no boundary at all.
///
/// `"jobobject"` names the win-native kill-on-close Job Object scoping, but it
/// probes Absent *everywhere* today: nothing assigns a pane's PTY process to a
/// job (`spawn_grouped` covers background tasks and agent runs, not panes), and
/// a backend that reports a containment boundary it never applies is a false
/// security claim. It stays in the chain so the entry keeps its place for when
/// pane spawn actually joins a job.
///
/// The OCI entries are ordinary candidates on native Windows: a Windows
/// `podman.exe`/`docker.exe` (Podman Desktop, Docker Desktop, Rancher) reaches
/// the same WSL2 machine directly, `sandbox::container_path` maps mount
/// destinations into the `/mnt/<drive>/…` tree, and `sandbox_gitshim` makes a
/// linked worktree's git metadata resolve under that mapping. `"apple"` is
/// macOS's `container` CLI, whose probe is `cfg!(target_os = "macos")`-gated so
/// a stray `container` binary on a Linux PATH can never be picked.
///
/// `apple` sits after `docker` and before `bwrap`: on a Mac the two OCI entries
/// ahead of it (podman/docker machine) are explicit user installs and keep
/// priority, `bwrap`/`systemd-run` are Linux-only, and without any of them the
/// chain still ends at `host`. Before `apple` was in this list, a Mac with
/// Apple's `container` installed silently resolved `auto` to an unsandboxed
/// host pane.
/// NB: `"wsl"` is still deliberately NOT in this chain — but no longer because
/// it is broken. The path translation it was waiting on has landed
/// ([`crate::sandbox::container_path`]), so its argv is correct now. It stays
/// out because on Windows a `podman.exe` / `docker.exe` from Podman or Docker
/// Desktop reaches the same WSL2 machine *directly*, without a second
/// `wsl.exe --` hop and without thegn guessing which distro. Those two are
/// already in the chain and probe for themselves. Opt into `wsl` explicitly
/// when you want a particular distro's runtime rather than the Desktop machine.
pub(crate) fn default_backend_chain() -> Vec<String> {
    [
        "podman-rootless",
        "podman-rootful",
        "docker",
        "apple",
        "bwrap",
        "appcontainer",
        "jobobject",
        "host",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
