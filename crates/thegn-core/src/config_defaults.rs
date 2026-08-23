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
/// platform, because each OS-native entry probes Absent off its own OS:
/// `"jobobject"` is the win-native kill-on-close Job Object scoping (the OCI
/// entries decline on native Windows — Linux containers can't bind-mount the
/// worktree at its real path; use WSL2 for container sandboxes), and `"apple"`
/// is macOS's `container` CLI, whose probe is `cfg!(target_os = "macos")`-gated
/// so a stray `container` binary on a Linux PATH can never be picked.
///
/// `apple` sits after `docker` and before `bwrap`: on a Mac the two OCI entries
/// ahead of it (podman/docker machine) are explicit user installs and keep
/// priority, `bwrap`/`systemd-run` are Linux-only, and without any of them the
/// chain still ends at `host`. Before `apple` was in this list, a Mac with
/// Apple's `container` installed silently resolved `auto` to an unsandboxed
/// host pane.
/// NB: `"wsl"` is deliberately NOT in this chain. `Backend::Wsl` exists and is
/// exempted from the "decline OCI on native Windows" rule, but its argv builder
/// is still aspirational — it hands a *Windows* worktree path to a Linux
/// container as `--workdir` without translating it, i.e. it has precisely the
/// bind-mount bug that rule exists to prevent. Auto-selecting it would silently
/// break the sandbox contract on any Windows box with WSL installed. Opt in
/// explicitly via `backend_chain` once the path translation lands.
pub(crate) fn default_backend_chain() -> Vec<String> {
    [
        "podman-rootless",
        "podman-rootful",
        "docker",
        "apple",
        "bwrap",
        "jobobject",
        "host",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
