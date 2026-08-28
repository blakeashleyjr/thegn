//! Finisher-prompt composition (THE-86): the pure half of
//! `session open --resume-work` — the prompt that asks a worker to FINISH a
//! stage a previous attempt left unfinished, rather than start it over.
//!
//! Doctrine, restated from [`crate::pipeline_run`]: everything here is
//! **pure** — no I/O, no subprocess, no filesystem, no tokio, no clock, no
//! environment, no randomness. The host gathers the facts (the row's artifact
//! state, the worktree's git state, the previous session's final screen) and
//! passes them in as plain data; this module only composes. Nothing here
//! advances a stage, judges the work, or touches the roster — the same
//! "structure, not judgment" line [`crate::config_pipeline`] draws, applied to
//! the resume side.

/// How many of the previous session's final **non-blank** screen lines the
/// finisher prompt embeds. The caller may pass more; [`finisher_prompt`]
/// truncates, so the prompt's size is bounded no matter how tall the screen
/// was.
pub const SCREEN_TAIL_LINES: usize = 8;

/// The facts one finisher prompt is composed from. Every field is plain data
/// the host gathered — the module holds no way to gather anything itself.
pub struct FinisherInput<'a> {
    /// The `[[pipeline.stages]]` name being resumed (e.g. `"code"`).
    pub stage_name: &'a str,
    /// The RENDERED original task — the stage template rendered against the
    /// row's own bindings, verbatim. The finisher reads it to know what
    /// "finished" means; the host rendered it, this module only embeds it.
    pub stage_prompt: &'a str,
    /// The row's handoff artifact path. `""` when the row carries none.
    pub artifact: &'a str,
    /// Whether the artifact file exists in the row's worktree (a regular
    /// file — the same symlink-strict rule the done gate applies).
    pub artifact_exists: bool,
    /// Whether git tracks the artifact (it is committed). Only meaningful
    /// when [`FinisherInput::artifact_exists`].
    pub artifact_tracked: bool,
    /// `git status --porcelain` output. `""` when the worktree is clean.
    pub git_status: &'a str,
    /// `git diff --stat` output. `""` when there is no unstaged diff.
    pub diff_stat: &'a str,
    /// The previous session's final screen, already split into lines. May
    /// hold more than [`SCREEN_TAIL_LINES`]; the prompt keeps the last
    /// non-blank [`SCREEN_TAIL_LINES`]. Empty ⇒ the prompt says the screen
    /// is unavailable instead of failing — a reaped tombstone or a session
    /// that never painted must not refuse the resume.
    pub screen_tail: &'a [String],
}

/// Compose the finisher prompt for one resumed dispatch.
///
/// Deterministic: the same input always yields the same bytes (no clock, no
/// randomness, no environment), so tests can assert exact output and a
/// retried resume does not silently reword the task.
///
/// Embedded text is sanitized: ANSI escape sequences and control characters
/// are stripped from every input before it reaches the prompt, so a hostile
/// tracker body or a noisy screen dump cannot smuggle terminal control
/// sequences into the next worker's context. Newlines and tabs survive (they
/// are layout, not control); `\r` does not.
pub fn finisher_prompt(i: &FinisherInput) -> String {
    let stage = sanitize(i.stage_name);
    let task = sanitize(i.stage_prompt);
    let artifact = sanitize(i.artifact);

    let mut p = String::with_capacity(1024);
    p.push_str(&format!(
        "You are the finisher for stage `{stage}` of a pipeline dispatch: a \
         previous worker attempted this stage and left it unfinished. \
         Complete the stage from where it stands — do not restart the task \
         from scratch.\n\n"
    ));
    p.push_str("The original task, as the previous worker received it (verbatim):\n\n");
    p.push_str(task.trim_end());
    p.push_str("\n\n");

    // The artifact-state paragraph: exactly one of the three states, never a
    // blend — the finisher must know unambiguously whether the handoff is
    // missing, uncommitted, or committed-but-maybe-stale.
    p.push_str(&format!(
        "Artifact state: {}\n\n",
        artifact_sentence(&artifact, i.artifact_exists, i.artifact_tracked)
    ));

    // Worktree facts, each in a fenced block — and only when non-empty: a
    // clean worktree renders no block rather than an empty one.
    let status = sanitize(i.git_status);
    let diff = sanitize(i.diff_stat);
    if !status.trim().is_empty() || !diff.trim().is_empty() {
        p.push_str("Worktree facts:\n\n");
        if !status.trim().is_empty() {
            p.push_str(&fenced_block("git status --porcelain", &status));
            p.push('\n');
        }
        if !diff.trim().is_empty() {
            p.push_str(&fenced_block("git diff --stat", &diff));
            p.push('\n');
        }
    }

    // The previous session's final screen, quoted line by line. Blank lines
    // are dropped first, then the last SCREEN_TAIL_LINES are kept, so a tall
    // or chatty screen cannot blow the prompt up.
    let tail: Vec<String> = nonblank_tail(i.screen_tail, SCREEN_TAIL_LINES);
    if tail.is_empty() {
        p.push_str(
            "The previous session's final screen is unavailable (no captured \
             output survived the session).\n\n",
        );
    } else {
        p.push_str(&format!(
            "The previous session's final screen (last {} non-blank lines):\n\n",
            SCREEN_TAIL_LINES
        ));
        for line in &tail {
            p.push_str("| ");
            p.push_str(line);
            p.push('\n');
        }
        p.push('\n');
    }

    // The closer: the exit-0-is-not-done rule and the commit instruction.
    // A worker that exits cleanly without a committed artifact is precisely
    // the failure the resume exists to recover from, so the rule is stated,
    // not implied.
    let target = if artifact.is_empty() {
        "the stage's handoff artifact".to_string()
    } else {
        format!("the handoff artifact at `{artifact}`")
    };
    p.push_str(&format!(
        "Finish the stage, do not merely exit: a zero exit code is not a done \
         stage — the `dispatch set-status done` gate refuses a handoff \
         artifact that git does not track. Leave {target} committed on the \
         branch before you end your turn.\n"
    ));
    p
}

/// The artifact-state sentence — exactly one of the three states the done
/// gate distinguishes. Split from [`finisher_prompt`] so the exact wording is
/// assertable on its own.
fn artifact_sentence(artifact: &str, exists: bool, tracked: bool) -> String {
    if !exists {
        format!("the handoff artifact `{artifact}` was never written")
    } else if !tracked {
        format!(
            "the handoff artifact `{artifact}` was written but NOT committed — \
             committing it is part of finishing"
        )
    } else {
        format!(
            "the handoff artifact `{artifact}` was already committed — verify \
             it is current before declaring the stage done"
        )
    }
}

/// One fenced command-output block (used only for non-empty bodies).
fn fenced_block(label: &str, body: &str) -> String {
    format!("```\n$ {label}\n{}\n```", body.trim_end_matches('\n'))
}

/// Keep the last `max` non-blank lines (blank lines dropped first, so a
/// screen of mostly padding cannot crowd real output out of the tail).
fn nonblank_tail(lines: &[String], max: usize) -> Vec<String> {
    lines
        .iter()
        .map(|l| sanitize(l).trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(max)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Strip ANSI escape sequences and control characters, keeping newlines and
/// tabs. CSI sequences (`ESC [ … final-byte`), OSC payloads (`ESC ] … BEL` or
/// `ESC ] … ESC \`), and two-byte escapes (`ESC c`) are consumed whole so no
/// partial sequence leaks; every other control character is dropped.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // Consume through the final byte (@–~) of the sequence.
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // Consume through the string terminator: BEL, or ESC \.
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    // Two-byte escape (e.g. `ESC c`): drop the follower.
                    chars.next();
                }
            }
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> FinisherInput<'static> {
        FinisherInput {
            stage_name: "code",
            stage_prompt: "Implement the parser fix; commit on this branch",
            artifact: ".thegn/pipeline/THE-86/code/7.md",
            artifact_exists: false,
            artifact_tracked: false,
            git_status: "",
            diff_stat: "",
            screen_tail: &[],
        }
    }

    #[test]
    fn a_missing_artifact_says_it_was_never_written() {
        let p = finisher_prompt(&input());
        assert!(
            p.contains("the handoff artifact `.thegn/pipeline/THE-86/code/7.md` was never written")
        );
    }

    #[test]
    fn an_untracked_artifact_says_committing_is_part_of_finishing() {
        let mut i = input();
        i.artifact_exists = true;
        i.artifact_tracked = false;
        let p = finisher_prompt(&i);
        assert!(p.contains(
            "the handoff artifact `.thegn/pipeline/THE-86/code/7.md` was written but NOT committed — committing it is part of finishing"
        ));
    }

    #[test]
    fn a_tracked_artifact_says_verify_it_is_current() {
        let mut i = input();
        i.artifact_exists = true;
        i.artifact_tracked = true;
        let p = finisher_prompt(&i);
        assert!(p.contains(
            "the handoff artifact `.thegn/pipeline/THE-86/code/7.md` was already committed — verify it is current before declaring the stage done"
        ));
    }

    #[test]
    fn status_and_diff_blocks_render_only_when_non_empty() {
        let absent = finisher_prompt(&input());
        assert!(!absent.contains("git status --porcelain"));
        assert!(!absent.contains("git diff --stat"));
        assert!(!absent.contains("Worktree facts:"));

        let mut i = input();
        i.git_status = " M src/lib.rs\n";
        let status_only = finisher_prompt(&i);
        assert!(status_only.contains("$ git status --porcelain"));
        assert!(status_only.contains("M src/lib.rs"));
        assert!(!status_only.contains("git diff --stat"));

        let mut i = input();
        i.diff_stat = " src/lib.rs | 2 +-\n";
        let diff_only = finisher_prompt(&i);
        assert!(diff_only.contains("$ git diff --stat"));
        assert!(!diff_only.contains("git status --porcelain"));

        let mut i = input();
        i.git_status = " M a\n";
        i.diff_stat = " a | 1 +\n";
        let both = finisher_prompt(&i);
        assert!(both.contains("$ git status --porcelain"));
        assert!(both.contains("$ git diff --stat"));
    }

    #[test]
    fn whitespace_only_status_and_diff_count_as_empty() {
        let mut i = input();
        i.git_status = "   \n\t\n";
        i.diff_stat = "\n";
        let p = finisher_prompt(&i);
        assert!(!p.contains("Worktree facts:"));
    }

    #[test]
    fn the_tail_is_truncated_to_the_last_screen_tail_lines_non_blank_lines() {
        let lines: Vec<String> = (1..=12).map(|n| format!("line {n}")).collect();
        let p = finisher_prompt(&FinisherInput {
            screen_tail: &lines,
            ..input()
        });
        for n in 1..=4 {
            assert!(!p.contains(&format!("\n| line {n}\n")));
        }
        for n in 5..=12 {
            assert!(p.contains(&format!("\n| line {n}\n")));
        }
        assert!(p.contains("(last 8 non-blank lines)"));
    }

    #[test]
    fn blank_tail_lines_are_dropped_before_truncation() {
        let lines: Vec<String> = (1..=10)
            .map(|n| {
                if n % 2 == 0 {
                    "   ".to_string()
                } else {
                    format!("line {n}")
                }
            })
            .collect();
        // 5 real lines — all survive, blanks excluded.
        let p = finisher_prompt(&FinisherInput {
            screen_tail: &lines,
            ..input()
        });
        assert!(p.contains("| line 1"));
        assert!(p.contains("| line 9"));
        assert_eq!(p.matches("\n| line ").count(), 5);
    }

    #[test]
    fn an_empty_everything_still_renders_and_says_the_screen_is_unavailable() {
        let i = FinisherInput {
            stage_name: "",
            stage_prompt: "",
            artifact: "",
            artifact_exists: false,
            artifact_tracked: false,
            git_status: "",
            diff_stat: "",
            screen_tail: &[],
        };
        let p = finisher_prompt(&i);
        assert!(p.contains("screen is unavailable"));
        assert!(p.contains("not a done stage"));
        assert!(!p.contains("`\u{1b}"));
    }

    #[test]
    fn the_prompt_is_deterministic() {
        let lines: Vec<String> = vec!["alpha".into(), "".into(), "beta".into()];
        let mk = || {
            finisher_prompt(&FinisherInput {
                git_status: " M x\n",
                diff_stat: " x | 1 +\n",
                screen_tail: &lines,
                ..input()
            })
        };
        assert_eq!(mk(), mk());
    }

    #[test]
    fn no_ansi_or_control_characters_pass_through() {
        let lines = vec!["\u{1b}[31mred\u{1b}[0m pane".to_string()];
        let p = finisher_prompt(&FinisherInput {
            stage_name: "co\u{1b}[1mde",
            stage_prompt: "task \u{1b}]0;title\u{7}body\r\nnext",
            artifact: "art\u{7}ifact",
            git_status: "\u{1b}[32m M x\u{1b}[0m",
            diff_stat: "x \u{1b}[36m| 1\u{1b}[0m",
            screen_tail: &lines,
            ..input()
        });
        assert!(!p.contains('\u{1b}'), "ESC leaked into the prompt");
        assert!(!p.contains('\r'), "CR leaked into the prompt");
        assert!(!p.contains('\u{7}'), "BEL leaked into the prompt");
        assert!(p.contains("red pane"));
        assert!(p.contains("M x"));
        assert!(p.contains("next"), "newline inside the task must survive");
        assert!(p.contains("artifact"), "sanitized artifact path");
        assert!(p.contains("code"), "sanitized stage name");
    }

    #[test]
    fn the_closer_carries_the_exit0_rule_and_the_commit_instruction() {
        let p = finisher_prompt(&input());
        assert!(p.contains("a zero exit code is not a done stage"));
        assert!(p.contains("committed on the branch"));
    }
}
