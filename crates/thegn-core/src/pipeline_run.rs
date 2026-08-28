//! Pipeline run policy — the **mechanism** half of the agent pipeline: the
//! pure functions a supervisor's dispatch, verification and wake steps are
//! built from.
//!
//! This module renders **no judgment**. It decides _whether a claim is true_
//! (the artifact is committed, the row is waitable, the settings file is
//! mergeable) — never _what to do about it_. That is the doctrine
//! [`crate::config_pipeline`] states as "structure, not judgment", applied to
//! the runtime side: nothing here advances a stage, moves `next` forward, or
//! fires a timeout. Deciding what to dispatch, whether a verified result is
//! good, and what to do next stays the supervising agent's.
//!
//! Everything here is **pure**: no I/O, no subprocess, no filesystem, no tokio,
//! and no [`crate::db::Db`]. Facts about git and the filesystem are gathered by
//! the host (which owns the worktree) and passed in as plain data. That keeps
//! thegn-core substrate-free and its 95% line-coverage gate satisfiable, and it
//! makes every rule below a table-testable function.

/// The tracker key with its `<provider>:` prefix stripped
/// (`linear:THE-76` → `THE-76`).
///
/// Everything after the **first** `:`; if that is empty (or there is no colon),
/// the whole string is used. The result is whitelist-sanitized — see
/// [`artifact_path`] for why — and an empty result falls back to `issue` so a
/// path component never disappears.
pub fn issue_key(issue_id: &str) -> String {
    let raw = match issue_id.split_once(':') {
        Some((_, after)) if !after.is_empty() => after,
        // No colon, or nothing after it (`"linear:"`) — use the whole string.
        _ => issue_id,
    };
    sanitize(raw, "issue")
}

/// `.thegn/pipeline/<ISSUE>/<stage>/<row>.md` — the per-issue handoff path.
///
/// `<ISSUE>` is [`issue_key`]; `<stage>` and the key are both
/// whitelist-sanitized to `[A-Za-z0-9._-]` (everything else maps to `-`, runs
/// of `-` collapse, leading/trailing `-`/`.` are trimmed, empty becomes
/// `issue`/`stage`). The row id disambiguates the parallel workers of one
/// stage.
///
/// **This is a security boundary, not cosmetics.** The result is joined under
/// a worktree path and written to, and a tracker id is attacker-adjacent data
/// (an id from a misconfigured provider is cheap to defend). No `/`, `\`,
/// control character or non-ASCII byte survives, so the path stays inside
/// `.thegn/pipeline/`; a bare `..` cannot survive because leading/trailing
/// `.` are trimmed — the component is either a plain filename or the fallback.
pub fn artifact_path(issue_id: &str, stage: &str, row_id: i64) -> String {
    format!(
        ".thegn/pipeline/{}/{}/{}.md",
        issue_key(issue_id),
        sanitize(stage, "stage"),
        row_id
    )
}

/// Whitelist-sanitize one path component: keep `[A-Za-z0-9._-]`, map every
/// other character (including `/`, `\`, whitespace, control chars, non-ASCII)
/// to `-`, collapse runs of `-`, trim leading/trailing `-` and `.`. An empty
/// result becomes `fallback`.
fn sanitize(raw: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The filesystem/git facts about one row's artifact, gathered by the host
/// (which owns the worktree) and passed to [`verify_report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFacts {
    /// The row's `artifact_path` column. `None` = a plain (non-pipeline)
    /// dispatch.
    pub artifact: Option<String>,
    /// The file exists under the worktree.
    pub exists: bool,
    /// git tracks the file (`git ls-files` names it).
    pub tracked: bool,
    /// The worktree has uncommitted changes.
    pub dirty: bool,
}

/// The verdict for one run-completion claim, plus the reasons a caller can
/// print verbatim on refusal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerifyReport {
    /// Whether `set-status done` may proceed.
    pub ok: bool,
    pub artifact: Option<String>,
    pub exists: bool,
    pub tracked: bool,
    pub dirty: bool,
    /// Only the things that make `ok` false — a caller prints this verbatim
    /// on refusal. `dirty` is deliberately *not* here: it never blocks.
    pub reasons: Vec<String>,
}

/// The run-completion verdict for one roster row.
///
/// Rules, and why:
///
/// - `artifact == None` ⇒ `ok`, no reasons. A row with no artifact is a plain
///   (non-pipeline) dispatch; the column has been optional since the roster
///   gained pipeline columns, and gating those would break `set-status done`
///   for every non-pipeline user while catching nothing.
/// - Otherwise `ok = exists && tracked`, with one reason per miss. The tracked
///   check is what catches the real pilot failure ("session exit ≠ done": the
///   worker wrote the file and never committed it) — git is the source of
///   truth, so an uncommitted artifact is not a handoff yet.
/// - A dirty worktree is **reported, never blocking**: it is legitimate
///   mid-review, and the tracked check already holds the line.
pub fn verify_report(f: &VerifyFacts) -> VerifyReport {
    let Some(a) = &f.artifact else {
        return VerifyReport {
            ok: true,
            artifact: None,
            exists: f.exists,
            tracked: f.tracked,
            dirty: f.dirty,
            reasons: Vec::new(),
        };
    };
    let mut reasons = Vec::new();
    if !f.exists {
        reasons.push(format!("artifact {a:?} does not exist under the worktree"));
    } else if !f.tracked {
        reasons.push(format!(
            "artifact {a:?} exists but git does not track it — commit it"
        ));
    }
    VerifyReport {
        ok: reasons.is_empty(),
        artifact: f.artifact.clone(),
        exists: f.exists,
        tracked: f.tracked,
        dirty: f.dirty,
        reasons,
    }
}

/// One row a wake step can wait on: its roster id, the daemon session whose
/// exit is the thing waited for, and the stage/issue context a wake message
/// needs to say what woke and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WaitTarget {
    pub id: i64,
    pub session_id: String,
    pub stage: Option<String>,
    pub issue_id: String,
}

/// Why a wait request has nothing (or nothing specific) to wait on.
/// Displayed in operator language; the host bails with these verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitSelectError {
    /// An explicit row id named a row the roster does not have.
    NoSuchRow(i64),
    /// An explicit row id named a row whose status is not waitable.
    NotActive(i64, &'static str),
    /// An explicit row id named a waitable row that has no session.
    NoSession(i64),
    /// The `--any` form found no waitable row with a live session.
    NoneActive,
}

impl std::fmt::Display for WaitSelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitSelectError::NoSuchRow(id) => {
                write!(
                    f,
                    "roster row {id} does not exist (see `thegn dispatch list`)"
                )
            }
            WaitSelectError::NotActive(id, status) => write!(
                f,
                "roster row {id} is {status:?}, not spawning/running — only a live \
                 worker can be waited on"
            ),
            WaitSelectError::NoSession(id) => write!(
                f,
                "roster row {id} has no session to wait on (it was not opened through \
                 `thegn session open`)"
            ),
            WaitSelectError::NoneActive => write!(
                f,
                "no spawning or running roster row carries a live session — nothing to wait on"
            ),
        }
    }
}

/// Select the rows a wake step (`dispatch wait`) can wait on.
///
/// - `Some(id)`: the row must exist, be waitable, and carry a non-empty
///   `session_id` — each miss is its own error variant so the message can name
///   the row and the reason.
/// - `None` (the `--any` case): every waitable row with a non-empty
///   `session_id`, in roster order. An empty selection is
///   [`WaitSelectError::NoneActive`].
///
/// **Waitable = `Spawning | Running` — deliberately narrower than
/// `AgentDispatchStatus::is_active`.** `Queued` has no session yet (it is a
/// row a supervisor re-drives, not a live worker), and `WaitingHuman`/`PrOpen`
/// are rows whose worker already finished: they are grouped under `is_active`
/// because that answers a different question — "don't re-dispatch this" — not
/// "is a process live to wait for". Including them would make `--any` return
/// instantly and forever, starving the real wait behind a parked row.
pub fn wait_candidates(
    rows: &[crate::issue::AgentDispatch],
    row: Option<i64>,
) -> Result<Vec<WaitTarget>, WaitSelectError> {
    let waitable = |s: crate::issue::AgentDispatchStatus| {
        matches!(
            s,
            crate::issue::AgentDispatchStatus::Spawning
                | crate::issue::AgentDispatchStatus::Running
        )
    };
    let target = |r: &crate::issue::AgentDispatch| WaitTarget {
        id: r.id,
        session_id: r.session_id.clone().unwrap_or_default(),
        stage: r.stage.clone(),
        issue_id: r.issue_id.clone(),
    };
    match row {
        Some(id) => {
            let r = rows
                .iter()
                .find(|r| r.id == id)
                .ok_or(WaitSelectError::NoSuchRow(id))?;
            if !waitable(r.status) {
                return Err(WaitSelectError::NotActive(id, r.status.as_str()));
            }
            let sid = r.session_id.as_deref().unwrap_or_default();
            if sid.is_empty() {
                return Err(WaitSelectError::NoSession(id));
            }
            Ok(vec![target(r)])
        }
        None => {
            let got: Vec<WaitTarget> = rows
                .iter()
                .filter(|r| {
                    waitable(r.status) && r.session_id.as_deref().is_some_and(|s| !s.is_empty())
                })
                .map(target)
                .collect();
            if got.is_empty() {
                Err(WaitSelectError::NoneActive)
            } else {
                Ok(got)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::{AgentDispatch, AgentDispatchStatus as S};

    // --- artifact paths ------------------------------------------------------

    #[test]
    fn a_provider_prefixed_id_becomes_a_bare_key() {
        assert_eq!(issue_key("linear:THE-76"), "THE-76");
        // The key is what the path is built from, so the prefix must not leak.
        let p = artifact_path("linear:THE-76", "code", 5);
        assert_eq!(p, ".thegn/pipeline/THE-76/code/5.md");
        // A colon-free id is used whole.
        assert_eq!(issue_key("THE-76"), "THE-76");
    }

    /// The boundary property: whatever goes in, the joined path stays under
    /// `.thegn/pipeline/` — no `/` or `\` inside a component, no component
    /// that is `.` or `..`, and nothing empty.
    fn assert_cannot_escape(raw: &str) {
        for (what, path) in [
            ("issue", artifact_path(raw, "code", 1)),
            ("stage", artifact_path("linear:THE-76", raw, 1)),
        ] {
            let rest = path
                .strip_prefix(".thegn/pipeline/")
                .unwrap_or_else(|| panic!("{what}: {raw:?} produced {path:?} outside the root"));
            let parts: Vec<&str> = rest.split('/').collect();
            assert_eq!(parts.len(), 3, "{what}: {raw:?} produced {path:?}");
            for part in parts.iter() {
                assert!(!part.is_empty(), "{what}: {raw:?} produced {path:?}");
                assert!(
                    !part.contains(['/', '\\']),
                    "{what}: {raw:?} produced {path:?}"
                );
                assert_ne!(*part, "..", "{what}: {raw:?} produced {path:?}");
                assert_ne!(*part, ".", "{what}: {raw:?} produced {path:?}");
            }
        }
    }

    #[test]
    fn a_traversal_attempt_cannot_escape_the_worktree() {
        for raw in ["linear:../../etc", "..", "a/../../b", "linear:", ""] {
            assert_cannot_escape(raw);
        }
        // Empty/malformed keys fall back rather than disappearing…
        assert_eq!(issue_key(""), "issue");
        assert_eq!(issue_key("linear:"), "linear");
        // …and a stage that sanitizes to nothing falls back too.
        assert_eq!(artifact_path("x", "..", 1), ".thegn/pipeline/x/stage/1.md");
    }

    #[test]
    fn the_row_id_disambiguates_two_coders_of_one_stage() {
        let a = artifact_path("linear:THE-76", "code", 5);
        let b = artifact_path("linear:THE-76", "code", 6);
        assert_ne!(a, b);
        assert!(a.ends_with("/5.md") && b.ends_with("/6.md"), "{a} {b}");
    }

    #[test]
    fn sanitization_is_idempotent() {
        for raw in [
            "linear:THE-76",
            "a b/c",
            "a/../../b",
            "..",
            "linear:",
            "??",
            "ok_name-2.0",
        ] {
            let once = issue_key(raw);
            assert_eq!(issue_key(&once), once, "issue_key({raw:?})");
        }
        for raw in ["code", "a b/c", "..", "??", "code-2"] {
            let once = sanitize(raw, "stage");
            assert_eq!(sanitize(&once, "stage"), once, "stage {raw:?}");
            // Through the public surface: the stage segment of the path is
            // already sanitized, so rebuilding it changes nothing.
            let p = artifact_path("linear:X", raw, 1);
            let seg = p.split('/').nth(3).unwrap(); // the stage segment
            assert_eq!(artifact_path("linear:X", seg, 1), p, "stage {raw:?}");
        }
    }

    // --- run-completion verdict ----------------------------------------------

    fn facts(artifact: Option<&str>, exists: bool, tracked: bool, dirty: bool) -> VerifyFacts {
        VerifyFacts {
            artifact: artifact.map(str::to_string),
            exists,
            tracked,
            dirty,
        }
    }

    #[test]
    fn a_row_without_an_artifact_is_never_gated() {
        let r = verify_report(&facts(None, false, false, true));
        assert!(r.ok);
        assert!(r.reasons.is_empty());
        assert_eq!(r.artifact, None);
    }

    #[test]
    fn a_missing_artifact_is_refused_with_a_reason() {
        let r = verify_report(&facts(
            Some(".thegn/pipeline/THE-76/code/5.md"),
            false,
            false,
            false,
        ));
        assert!(!r.ok);
        assert_eq!(r.reasons.len(), 1);
        assert!(
            r.reasons[0].contains("does not exist under the worktree")
                && r.reasons[0].contains(".thegn/pipeline/THE-76/code/5.md"),
            "{r:?}"
        );
    }

    #[test]
    fn an_untracked_artifact_is_refused_and_the_reason_says_commit_it() {
        let r = verify_report(&facts(Some("a.md"), true, false, false));
        assert!(!r.ok);
        assert!(
            r.reasons[0].contains("does not track it — commit it"),
            "{r:?}"
        );
    }

    #[test]
    fn a_dirty_worktree_is_reported_but_never_blocks() {
        let clean = verify_report(&facts(Some("a.md"), true, true, false));
        assert!(clean.ok);
        let dirty = verify_report(&facts(Some("a.md"), true, true, true));
        assert!(dirty.ok, "dirty must never block");
        assert!(dirty.dirty);
        assert!(dirty.reasons.is_empty(), "reasons are only the blockers");
        // …and a refusal's dirty flag is still just a report.
        let both = verify_report(&facts(Some("a.md"), true, false, true));
        assert!(!both.ok && both.dirty && both.reasons.len() == 1);
    }

    #[test]
    fn a_present_tracked_artifact_passes() {
        let r = verify_report(&facts(
            Some(".thegn/pipeline/THE-76/code/5.md"),
            true,
            true,
            false,
        ));
        assert!(r.ok);
        assert!(r.reasons.is_empty());
        assert_eq!(
            r.artifact.as_deref(),
            Some(".thegn/pipeline/THE-76/code/5.md")
        );
    }

    // --- wait-target selection -------------------------------------------------

    fn row(
        id: i64,
        issue: &str,
        stage: Option<&str>,
        status: S,
        session: Option<&str>,
    ) -> AgentDispatch {
        AgentDispatch {
            id,
            issue_id: issue.to_string(),
            worktree_path: format!("/wt/{id}"),
            agent_name: "claude".to_string(),
            dispatched_at_ms: id,
            status,
            stage: stage.map(str::to_string),
            parent_id: None,
            session_id: session.map(str::to_string),
            artifact_path: None,
            note: None,
        }
    }

    #[test]
    fn only_spawning_and_running_rows_are_waited_on() {
        let rows = vec![
            row(1, "linear:A-1", Some("code"), S::Queued, Some("s1")),
            row(2, "linear:A-1", Some("code"), S::Spawning, Some("s2")),
            row(3, "linear:A-1", Some("code"), S::Running, Some("s3")),
            row(4, "linear:A-1", Some("code"), S::WaitingHuman, Some("s4")),
            row(5, "linear:A-1", Some("code"), S::PrOpen, Some("s5")),
            row(6, "linear:A-1", Some("code"), S::Done, Some("s6")),
        ];
        let got = wait_candidates(&rows, None).unwrap();
        let ids: Vec<i64> = got.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![2, 3], "roster order, waitable only");
    }

    #[test]
    fn a_row_without_a_session_is_named_not_silently_skipped() {
        // An explicit id gets a specific error naming the row…
        let rows = vec![row(7, "linear:A-1", Some("code"), S::Running, None)];
        assert_eq!(
            wait_candidates(&rows, Some(7)),
            Err(WaitSelectError::NoSession(7))
        );
        // …while --any skips it silently (its wake would never fire anyway).
        assert!(wait_candidates(&rows, None).is_err());
    }

    #[test]
    fn an_empty_roster_reports_nothing_active() {
        assert_eq!(wait_candidates(&[], None), Err(WaitSelectError::NoneActive));
        assert_eq!(
            wait_candidates(&[], Some(3)),
            Err(WaitSelectError::NoSuchRow(3))
        );
    }

    #[test]
    fn selecting_one_row_reports_why_it_is_unwaitable() {
        let rows = vec![
            row(1, "linear:A-1", Some("code"), S::Running, Some("s1")),
            row(2, "linear:A-1", Some("code"), S::WaitingHuman, Some("s2")),
            row(3, "linear:A-1", Some("code"), S::Spawning, None),
        ];
        // Not a row at all.
        assert_eq!(
            wait_candidates(&rows, Some(99)),
            Err(WaitSelectError::NoSuchRow(99))
        );
        // A row whose worker already finished (parked, not live).
        assert_eq!(
            wait_candidates(&rows, Some(2)),
            Err(WaitSelectError::NotActive(2, "waiting_human"))
        );
        // A live row with no session to wait on.
        assert_eq!(
            wait_candidates(&rows, Some(3)),
            Err(WaitSelectError::NoSession(3))
        );
        // The happy path returns exactly that row.
        assert_eq!(wait_candidates(&rows, Some(1)).unwrap()[0].session_id, "s1");
    }

    #[test]
    fn candidates_carry_the_stage_and_issue_for_the_wake_message() {
        let rows = vec![
            row(1, "linear:A-1", None, S::Running, Some("s1")),
            row(2, "linear:B-2", Some("review"), S::Running, Some("s2")),
        ];
        let got = wait_candidates(&rows, None).unwrap();
        assert_eq!(got[0].stage, None);
        assert_eq!(got[0].issue_id, "linear:A-1");
        assert_eq!(got[1].stage.as_deref(), Some("review"));
        assert_eq!(got[1].issue_id, "linear:B-2");
    }
}
