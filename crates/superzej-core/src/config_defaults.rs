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
