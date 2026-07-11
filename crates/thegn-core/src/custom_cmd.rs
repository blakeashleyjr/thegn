//! Template expansion for lazygit-style user-defined custom commands:
//! `git push {{.SelectedBranch.Name | quote}}` with dotted paths resolved
//! against the current UI selection (and prompt responses collected first).
//!
//! Pure string → string; config parsing and execution live elsewhere. A
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
    UnknownFilter(String),
    /// Malformed placeholder: unterminated `{{`, empty body, non-dotted path.
    Syntax(String),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPath(p) => write!(f, "unknown template path '{p}'"),
            Self::MissingValue(p) => write!(f, "no value for '{p}' in the current selection"),
            Self::UnknownFilter(x) => write!(f, "unknown template filter '{x}'"),
            Self::Syntax(m) => write!(f, "template syntax error: {m}"),
        }
    }
}

impl std::error::Error for TemplateError {}

/// Expand `{{ .Dotted.Path }}` and `{{ .Dotted.Path | quote }}` placeholders.
///
/// Whitespace inside the braces is flexible (`{{.X}}`, `{{ .X }}`,
/// `{{ .X | quote }}`). Text outside placeholders passes through verbatim,
/// including lone `{` / `}` (so `awk '{print $1}'` survives). The only filter
/// is `quote` ([`crate::util::sh_quote`]). Unknown paths, missing selection
/// values, unknown filters and malformed placeholders are all hard errors.
pub fn expand(template: &str, ctx: &TemplateCtx) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| TemplateError::Syntax("unterminated '{{'".to_string()))?;
        out.push_str(&eval_placeholder(after[..end].trim(), ctx)?);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Evaluate one trimmed placeholder body: `.Dotted.Path [| filter]`.
fn eval_placeholder(body: &str, ctx: &TemplateCtx) -> Result<String, TemplateError> {
    if body.is_empty() {
        return Err(TemplateError::Syntax("empty placeholder".to_string()));
    }
    let (path, filter) = match body.split_once('|') {
        Some((p, f)) => (p.trim_end(), Some(f.trim())),
        None => (body, None),
    };
    if !path.starts_with('.') {
        return Err(TemplateError::Syntax(format!(
            "expected a dotted path, got '{path}'"
        )));
    }
    // Resolve before checking the filter so a path error wins.
    let value = resolve(path, ctx)?;
    match filter {
        None => Ok(value),
        Some("quote") => Ok(crate::util::sh_quote(&value)),
        Some(other) => Err(TemplateError::UnknownFilter(other.to_string())),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-populated context so every path resolves.
    fn full_ctx() -> TemplateCtx {
        TemplateCtx {
            selected_commit: Some(CommitVars {
                sha: "deadbeefcafe".into(),
                short: "deadbee".into(),
                subject: "fix: the thing".into(),
                author: "Ada Lovelace".into(),
            }),
            selected_branch: Some(BranchVars {
                name: "feat/x".into(),
                upstream: Some("origin/feat/x".into()),
            }),
            checked_out_branch: Some(BranchVars {
                name: "main".into(),
                upstream: Some("origin/main".into()),
            }),
            selected_file: Some("src/lib.rs".into()),
            selected_stash: Some(StashVars {
                index: 2,
                message: "WIP on main".into(),
            }),
            worktree_path: Some("/home/u/wt".into()),
            prompt_responses: BTreeMap::from([("Remote".to_string(), "origin".to_string())]),
        }
    }

    fn one(template: &str) -> String {
        expand(template, &full_ctx()).unwrap()
    }

    #[test]
    fn passthrough_no_placeholders() {
        let t = "git log --oneline | awk '{print $1}' && echo {x} }y{";
        assert_eq!(one(t), t);
        assert_eq!(expand("", &TemplateCtx::default()).unwrap(), "");
    }

    #[test]
    fn commit_paths_and_aliases() {
        assert_eq!(one("{{.SelectedCommit.Sha}}"), "deadbeefcafe");
        assert_eq!(one("{{.SelectedCommit.Short}}"), "deadbee");
        assert_eq!(one("{{.SelectedCommit.Subject}}"), "fix: the thing");
        assert_eq!(one("{{.SelectedCommit.Author}}"), "Ada Lovelace");
        assert_eq!(one("{{.SelectedLocalCommit.Sha}}"), "deadbeefcafe");
        assert_eq!(one("{{.SelectedLocalCommit.Short}}"), "deadbee");
        assert_eq!(one("{{.SelectedLocalCommit.Subject}}"), "fix: the thing");
        assert_eq!(one("{{.SelectedLocalCommit.Author}}"), "Ada Lovelace");
    }

    #[test]
    fn branch_paths_and_aliases() {
        assert_eq!(one("{{.SelectedBranch.Name}}"), "feat/x");
        assert_eq!(one("{{.SelectedBranch.Upstream}}"), "origin/feat/x");
        assert_eq!(one("{{.SelectedLocalBranch.Name}}"), "feat/x");
        assert_eq!(one("{{.SelectedLocalBranch.Upstream}}"), "origin/feat/x");
        assert_eq!(one("{{.CheckedOutBranch.Name}}"), "main");
        assert_eq!(one("{{.CheckedOutBranch.Upstream}}"), "origin/main");
    }

    #[test]
    fn file_stash_worktree_paths_and_aliases() {
        assert_eq!(one("{{.SelectedFile}}"), "src/lib.rs");
        assert_eq!(one("{{.SelectedFile.Name}}"), "src/lib.rs");
        assert_eq!(one("{{.SelectedStash.Index}}"), "2");
        assert_eq!(one("{{.SelectedStash.Message}}"), "WIP on main");
        assert_eq!(one("{{.SelectedStashEntry.Index}}"), "2");
        assert_eq!(one("{{.SelectedStashEntry.Message}}"), "WIP on main");
        assert_eq!(one("{{.WorktreePath}}"), "/home/u/wt");
    }

    #[test]
    fn form_key_and_prompts_alias() {
        assert_eq!(one("git push {{.Form.Remote}}"), "git push origin");
        assert_eq!(one("git push {{.Prompts.Remote}}"), "git push origin");
    }

    #[test]
    fn quote_filter_uses_sh_quote() {
        let mut ctx = full_ctx();
        ctx.selected_commit.as_mut().unwrap().subject = "don't break 'this'".into();
        assert_eq!(
            expand("echo {{.SelectedCommit.Subject | quote}}", &ctx).unwrap(),
            r"echo 'don'\''t break '\''this'\'''"
        );
        // A bare word stays unquoted (sh_quote semantics).
        assert_eq!(one("{{.CheckedOutBranch.Name | quote}}"), "main");
        // A value with spaces gets wrapped.
        assert_eq!(one("{{.SelectedStash.Message | quote}}"), "'WIP on main'");
    }

    #[test]
    fn whitespace_variants() {
        assert_eq!(one("{{.WorktreePath}}"), "/home/u/wt");
        assert_eq!(one("{{ .WorktreePath }}"), "/home/u/wt");
        assert_eq!(one("{{\t.WorktreePath\t}}"), "/home/u/wt");
        assert_eq!(one("{{ .WorktreePath|quote }}"), "/home/u/wt");
        assert_eq!(
            one("{{  .SelectedStash.Message  |  quote  }}"),
            "'WIP on main'"
        );
    }

    #[test]
    fn multiple_and_adjacent_placeholders() {
        assert_eq!(
            one("git rebase {{.SelectedBranch.Name}} onto {{.CheckedOutBranch.Name}}"),
            "git rebase feat/x onto main"
        );
        assert_eq!(
            one("{{.SelectedCommit.Short}}{{.SelectedStash.Index}}"),
            "deadbee2"
        );
    }

    #[test]
    fn unknown_path_errors() {
        let err = expand("{{.SelectedFoo.Bar}}", &full_ctx()).unwrap_err();
        assert_eq!(err, TemplateError::UnknownPath(".SelectedFoo.Bar".into()));
        // Form/Prompts with no key are unknown paths, not missing values.
        assert_eq!(
            expand("{{.Form.}}", &full_ctx()).unwrap_err(),
            TemplateError::UnknownPath(".Form.".into())
        );
        assert_eq!(
            expand("{{.Form}}", &full_ctx()).unwrap_err(),
            TemplateError::UnknownPath(".Form".into())
        );
    }

    #[test]
    fn missing_value_each_empty_field() {
        let ctx = TemplateCtx::default();
        for path in [
            ".SelectedCommit.Sha",
            ".SelectedCommit.Short",
            ".SelectedCommit.Subject",
            ".SelectedCommit.Author",
            ".SelectedBranch.Name",
            ".SelectedBranch.Upstream",
            ".CheckedOutBranch.Name",
            ".CheckedOutBranch.Upstream",
            ".SelectedFile",
            ".SelectedStash.Index",
            ".SelectedStash.Message",
            ".WorktreePath",
            ".Form.Remote",
        ] {
            assert_eq!(
                expand(&format!("{{{{{path}}}}}"), &ctx).unwrap_err(),
                TemplateError::MissingValue(path.into()),
                "path {path}"
            );
        }
    }

    #[test]
    fn branch_present_but_no_upstream_is_missing_value() {
        let mut ctx = full_ctx();
        ctx.selected_branch.as_mut().unwrap().upstream = None;
        ctx.checked_out_branch.as_mut().unwrap().upstream = None;
        assert_eq!(
            expand("{{.SelectedBranch.Upstream}}", &ctx).unwrap_err(),
            TemplateError::MissingValue(".SelectedBranch.Upstream".into())
        );
        assert_eq!(
            expand("{{.CheckedOutBranch.Upstream}}", &ctx).unwrap_err(),
            TemplateError::MissingValue(".CheckedOutBranch.Upstream".into())
        );
        // Names still resolve.
        assert_eq!(expand("{{.SelectedBranch.Name}}", &ctx).unwrap(), "feat/x");
    }

    #[test]
    fn missing_form_key() {
        assert_eq!(
            expand("{{.Form.Nope}}", &full_ctx()).unwrap_err(),
            TemplateError::MissingValue(".Form.Nope".into())
        );
        assert_eq!(
            expand("{{.Prompts.Nope}}", &full_ctx()).unwrap_err(),
            TemplateError::MissingValue(".Prompts.Nope".into())
        );
    }

    #[test]
    fn unknown_filter() {
        assert_eq!(
            expand("{{.WorktreePath | upper}}", &full_ctx()).unwrap_err(),
            TemplateError::UnknownFilter("upper".into())
        );
        // Empty filter after the pipe is unknown too.
        assert_eq!(
            expand("{{.WorktreePath |}}", &full_ctx()).unwrap_err(),
            TemplateError::UnknownFilter(String::new())
        );
    }

    #[test]
    fn filter_on_unknown_path_path_error_wins() {
        assert_eq!(
            expand("{{.SelectedFoo.Bar | bogus}}", &full_ctx()).unwrap_err(),
            TemplateError::UnknownPath(".SelectedFoo.Bar".into())
        );
    }

    #[test]
    fn syntax_errors() {
        assert!(matches!(
            expand("echo {{.WorktreePath", &full_ctx()).unwrap_err(),
            TemplateError::Syntax(_)
        ));
        assert!(matches!(
            expand("{{}}", &full_ctx()).unwrap_err(),
            TemplateError::Syntax(_)
        ));
        assert!(matches!(
            expand("{{   }}", &full_ctx()).unwrap_err(),
            TemplateError::Syntax(_)
        ));
        // A body that isn't a dotted path.
        assert!(matches!(
            expand("{{WorktreePath}}", &full_ctx()).unwrap_err(),
            TemplateError::Syntax(_)
        ));
    }

    #[test]
    fn error_display() {
        assert_eq!(
            TemplateError::UnknownPath(".X".into()).to_string(),
            "unknown template path '.X'"
        );
        assert_eq!(
            TemplateError::MissingValue(".X".into()).to_string(),
            "no value for '.X' in the current selection"
        );
        assert_eq!(
            TemplateError::UnknownFilter("up".into()).to_string(),
            "unknown template filter 'up'"
        );
        assert_eq!(
            TemplateError::Syntax("boom".into()).to_string(),
            "template syntax error: boom"
        );
    }
}
