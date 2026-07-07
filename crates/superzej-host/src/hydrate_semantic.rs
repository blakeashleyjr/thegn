//! Semantic (entity-level) summary of a worktree's pending changes, computed
//! on the hydration thread — extracted from `hydrate.rs` (at the god-file
//! hard cap). One `git diff HEAD` subprocess + a tree-sitter parse per
//! changed file, capped so a sprawling change never balloons hydration.

/// Compute the entity-level summary of a worktree's pending changes from
/// `git diff HEAD` (semantic git layer). Runs on the hydration thread: one diff
/// subprocess + a tree-sitter parse per changed file. Capped at 50 files so a
/// sprawling change never balloons hydration; `None` when there's nothing to
/// show or git/parse yields no entities.
pub(crate) fn compute_entity_summary(
    loc: &superzej_core::remote::GitLoc,
    diff_entries: &[superzej_svc::git::DiffEntry],
) -> Option<superzej_core::semantic::EntitySummary> {
    use superzej_core::semantic::{EntitySummary, Lang, entities_for_diff};
    if diff_entries.is_empty() || diff_entries.len() > 50 {
        return None;
    }
    // Same sanitized flags the git backend uses (see svc SANITIZED_DIFF) so the
    // patch parses cleanly: no color/ext-diff/renames, 3 lines of context.
    let diff = loc.git_out(&[
        "-c",
        "diff.noprefix=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-renames",
        "-U3",
        "HEAD",
    ])?;
    let root = loc.path();
    let mut per_file = Vec::new();
    for f in superzej_core::patch::parse_patch(&diff) {
        let Some(lang) = Lang::from_path(&f.new_path) else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(std::path::Path::new(&root).join(&f.new_path)) else {
            continue;
        };
        let changes = entities_for_diff(&src, lang, &f.hunks);
        if !changes.is_empty() {
            per_file.push((f.new_path.clone(), changes));
        }
    }
    let summary = EntitySummary::new(per_file);
    (!summary.per_file.is_empty()).then_some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end over the I/O seam: a real temp git repo with an edited
    /// entity-bearing file → `compute_entity_summary` parses the diff + source
    /// and attributes churn to the function.
    #[test]
    fn compute_entity_summary_over_a_real_repo() {
        use superzej_core::util::{git_cmd, git_out};
        let dir = std::env::temp_dir().join(format!("sz-sem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // test code: fixture setup, never on the event loop.
        #[expect(clippy::disallowed_methods)]
        let run = |args: &[&str]| {
            assert!(
                git_cmd(&dir).args(args).status().unwrap().success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t.t"]);
        run(&["config", "user.name", "t"]);
        // Hermetic vs the developer's global gitconfig (a signing key isn't
        // reachable in sandboxed test runs).
        run(&["config", "commit.gpgsign", "false"]);
        let file = dir.join("lib.rs");
        std::fs::write(&file, "fn greet() -> u8 {\n    1\n}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        // Edit the function body → a real `git diff HEAD`.
        std::fs::write(&file, "fn greet() -> u8 {\n    42\n}\n").unwrap();

        let loc = superzej_core::remote::GitLoc::for_worktree(&dir);
        // A non-empty diff_entries list (only its length gates the call).
        let entries = vec![superzej_svc::git::DiffEntry {
            path: "lib.rs".into(),
            added: 1,
            deleted: 1,
        }];
        let summary = compute_entity_summary(&loc, &entries).expect("entity summary");
        assert_eq!(summary.per_file.len(), 1, "{summary:?}");
        let (path, changes) = &summary.per_file[0];
        assert_eq!(path, "lib.rs");
        assert_eq!(changes[0].name, "greet");
        assert!(changes[0].added > 0 && changes[0].deleted > 0);
        let impact = summary.impact.expect("impact");
        assert_eq!(impact.entities, 1);

        // A clean repo (no diff vs HEAD) yields None. `git_out` returns None on
        // empty output, so a clean tree shows no changed names.
        run(&["checkout", "--", "lib.rs"]);
        assert!(git_out(&dir, &["diff", "--name-only", "HEAD"]).is_none());
        assert!(compute_entity_summary(&loc, &entries).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
