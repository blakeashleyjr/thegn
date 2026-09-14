//! Template expansion for lazygit-style user-defined custom commands:
//! `git push {{.SelectedBranch.Name | quote}}` with dotted paths resolved
//! against the current UI selection (and prompt responses collected first).
//!
//! Compiled commands keep argument data separate from trusted shell syntax. A
//! referenced-but-missing value is a hard error, never a silent empty string —
//! that's how people force-push the wrong branch.

use std::collections::BTreeMap;
use std::fmt;

/// The selection context a template is expanded against. All fields optional —
/// referencing a missing one yields [`TemplateError::MissingValue`].
#[derive(Debug, Clone, Default)]
pub struct TemplateCtx {
    pub selected_commit: Option<CommitVars>,
    pub selected_branch: Option<BranchVars>,
    pub checked_out_branch: Option<BranchVars>,
    /// Repo-relative path of the selected file.
    pub selected_file: Option<String>,
    pub selected_stash: Option<StashVars>,
    pub worktree_path: Option<String>,
    /// Prompt answers keyed by the prompt's config key (`.Form.<Key>`).
    pub prompt_responses: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct CommitVars {
    pub sha: String,
    pub short: String,
    pub subject: String,
    pub author: String,
}

#[derive(Debug, Clone, Default)]
pub struct BranchVars {
    pub name: String,
    pub upstream: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StashVars {
    pub index: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// Syntactically valid dotted path that isn't in the path table.
    UnknownPath(String),
    /// Known path, but the selection doesn't carry a value for it.
    MissingValue(String),
    Policy(&'static str),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPath(p) => write!(f, "unknown template path '{p}'"),
            Self::MissingValue(p) => write!(f, "no value for '{p}' in the current selection"),
            Self::Policy(m) => write!(f, "custom command: {m}"),
        }
    }
}

impl std::error::Error for TemplateError {}

mod command;
pub use command::{ExpandedCommand, compile, validate_commands};
#[cfg(test)]
mod command_tests;

/// Resolve a dotted path against the context. Case-sensitive; accepts the
/// lazygit alias spellings alongside the canonical thegn names.
fn resolve(path: &str, ctx: &TemplateCtx) -> Result<String, TemplateError> {
    let missing = || TemplateError::MissingValue(path.to_string());
    if let Some(key) = path
        .strip_prefix(".Form.")
        .or_else(|| path.strip_prefix(".Prompts."))
    {
        if key.is_empty() {
            return Err(TemplateError::UnknownPath(path.to_string()));
        }
        return ctx.prompt_responses.get(key).cloned().ok_or_else(missing);
    }
    let commit = |get: fn(&CommitVars) -> &str| {
        ctx.selected_commit
            .as_ref()
            .map(|c| get(c).to_string())
            .ok_or_else(missing)
    };
    match path {
        ".SelectedCommit.Sha" | ".SelectedLocalCommit.Sha" => commit(|c| &c.sha),
        ".SelectedCommit.Short" | ".SelectedLocalCommit.Short" => commit(|c| &c.short),
        ".SelectedCommit.Subject" | ".SelectedLocalCommit.Subject" => commit(|c| &c.subject),
        ".SelectedCommit.Author" | ".SelectedLocalCommit.Author" => commit(|c| &c.author),
        ".SelectedBranch.Name" | ".SelectedLocalBranch.Name" => ctx
            .selected_branch
            .as_ref()
            .map(|b| b.name.clone())
            .ok_or_else(missing),
        ".SelectedBranch.Upstream" | ".SelectedLocalBranch.Upstream" => ctx
            .selected_branch
            .as_ref()
            .and_then(|b| b.upstream.clone())
            .ok_or_else(missing),
        ".CheckedOutBranch.Name" => ctx
            .checked_out_branch
            .as_ref()
            .map(|b| b.name.clone())
            .ok_or_else(missing),
        ".CheckedOutBranch.Upstream" => ctx
            .checked_out_branch
            .as_ref()
            .and_then(|b| b.upstream.clone())
            .ok_or_else(missing),
        ".SelectedFile" | ".SelectedFile.Name" => ctx.selected_file.clone().ok_or_else(missing),
        ".SelectedStash.Index" | ".SelectedStashEntry.Index" => ctx
            .selected_stash
            .as_ref()
            .map(|s| s.index.to_string())
            .ok_or_else(missing),
        ".SelectedStash.Message" | ".SelectedStashEntry.Message" => ctx
            .selected_stash
            .as_ref()
            .map(|s| s.message.clone())
            .ok_or_else(missing),
        ".WorktreePath" => ctx.worktree_path.clone().ok_or_else(missing),
        _ => Err(TemplateError::UnknownPath(path.to_string())),
    }
}
