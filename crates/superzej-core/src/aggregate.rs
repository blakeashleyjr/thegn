//! Cross-worktree aggregation — a multibuffer-style excerpt stream.
//!
//! superzej normally shows only the *active* worktree's state, but attention is
//! scattered: CI fails on one branch, another has uncommitted changes, a grep
//! hit lives in a third. Borrowing Zed's **multibuffer** idea (collect results
//! from many sources into one stream of excerpts, each a window onto its
//! source), this module models a read-only cross-worktree surface.
//!
//! Everything here is pure and unit-tested: it holds already-fetched excerpts,
//! orders them deterministically, groups them by worktree, exposes a flattened
//! rows view for cursor navigation, and resolves an excerpt back to its owning
//! worktree (the jump target). The DB/git reads that produce excerpts live in
//! the host; the pure builders below turn that fetched data into [`Excerpt`]s.

use crate::ci::CiRun;

/// What a cross-worktree excerpt came from. The discriminant order is the
/// intra-worktree sort severity (failures first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExcerptKind {
    /// A failing CI run.
    CiFailure,
    /// A file with uncommitted changes.
    DirtyFile,
    /// A content (grep) match.
    ContentMatch,
}

impl ExcerptKind {
    /// A short glyph-free tag for rendering.
    pub fn tag(self) -> &'static str {
        match self {
            ExcerptKind::CiFailure => "ci",
            ExcerptKind::DirtyFile => "dirty",
            ExcerptKind::ContentMatch => "match",
        }
    }
}

/// One item needing attention somewhere, a window onto its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    /// Owning worktree path — the jump target.
    pub worktree: String,
    /// Display label for the worktree (branch / repo).
    pub worktree_label: String,
    pub kind: ExcerptKind,
    /// Source file (repo-relative), or empty when not file-scoped (e.g. a run).
    pub file: String,
    pub line: Option<u64>,
    /// The excerpt text (matched line, run name, …).
    pub text: String,
    /// Secondary detail (url, status, count), best-effort.
    pub detail: String,
}

/// A flattened navigation row: a per-worktree divider, or an excerpt (by index
/// into the aggregation's sorted excerpts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggRow {
    Group { label: String, count: usize },
    Excerpt(usize),
}

/// Per-kind rollup counts for a summary line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AggSummary {
    pub failures: usize,
    pub dirty: usize,
    pub matches: usize,
    /// Distinct worktrees represented.
    pub worktrees: usize,
}

/// A cross-worktree excerpt stream, stored pre-sorted + grouped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aggregation {
    excerpts: Vec<Excerpt>,
}

impl Aggregation {
    /// Build from raw excerpts, sorting deterministically by
    /// `(worktree_label, kind severity, file, line, text)` so groups appear in
    /// label order with failures first inside each group, stable across runs.
    pub fn from_excerpts(mut excerpts: Vec<Excerpt>) -> Aggregation {
        excerpts.sort_by(|a, b| {
            a.worktree_label
                .cmp(&b.worktree_label)
                .then(a.kind.cmp(&b.kind))
                .then(a.file.cmp(&b.file))
                .then(a.line.cmp(&b.line))
                .then(a.text.cmp(&b.text))
        });
        Aggregation { excerpts }
    }

    pub fn excerpts(&self) -> &[Excerpt] {
        &self.excerpts
    }

    pub fn is_empty(&self) -> bool {
        self.excerpts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.excerpts.len()
    }

    /// The flattened rows view: each worktree group is introduced by a
    /// `Group` divider (label + count), followed by `Excerpt(index)` rows.
    pub fn rows(&self) -> Vec<AggRow> {
        let mut rows = Vec::new();
        let mut i = 0;
        while i < self.excerpts.len() {
            let label = &self.excerpts[i].worktree_label;
            let count = self.excerpts[i..]
                .iter()
                .take_while(|e| &e.worktree_label == label)
                .count();
            rows.push(AggRow::Group {
                label: label.clone(),
                count,
            });
            for j in i..i + count {
                rows.push(AggRow::Excerpt(j));
            }
            i += count;
        }
        rows
    }

    /// Resolve an excerpt (by its flat index, as carried in [`AggRow::Excerpt`])
    /// back to its source — used by "open at source".
    pub fn jump_target(&self, flat: usize) -> Option<&Excerpt> {
        self.excerpts.get(flat)
    }

    /// Per-kind counts + distinct worktrees.
    pub fn summary(&self) -> AggSummary {
        let mut s = AggSummary::default();
        let mut seen: Vec<&str> = Vec::new();
        for e in &self.excerpts {
            match e.kind {
                ExcerptKind::CiFailure => s.failures += 1,
                ExcerptKind::DirtyFile => s.dirty += 1,
                ExcerptKind::ContentMatch => s.matches += 1,
            }
            if !seen.contains(&e.worktree.as_str()) {
                seen.push(&e.worktree);
            }
        }
        s.worktrees = seen.len();
        s
    }
}

// --- pure builders ---------------------------------------------------------

/// One excerpt per *failing* CI run for a worktree (non-failures are dropped).
pub fn ci_failure_excerpts(worktree: &str, label: &str, runs: &[CiRun]) -> Vec<Excerpt> {
    runs.iter()
        .filter(|r| r.state.is_failure())
        .map(|r| Excerpt {
            worktree: worktree.to_string(),
            worktree_label: label.to_string(),
            kind: ExcerptKind::CiFailure,
            file: String::new(),
            line: None,
            text: if r.name.is_empty() {
                r.id.clone()
            } else {
                r.name.clone()
            },
            detail: if r.url.is_empty() {
                r.branch.clone()
            } else {
                r.url.clone()
            },
        })
        .collect()
}

/// One excerpt per dirty file (`(path, status)` pairs).
pub fn dirty_file_excerpts(
    worktree: &str,
    label: &str,
    files: &[(String, String)],
) -> Vec<Excerpt> {
    files
        .iter()
        .map(|(path, status)| Excerpt {
            worktree: worktree.to_string(),
            worktree_label: label.to_string(),
            kind: ExcerptKind::DirtyFile,
            file: path.clone(),
            line: None,
            text: path.clone(),
            detail: status.clone(),
        })
        .collect()
}

/// One excerpt per content (grep) match (`(file, line, text)` tuples).
pub fn content_match_excerpts(
    worktree: &str,
    label: &str,
    matches: &[(String, u64, String)],
) -> Vec<Excerpt> {
    matches
        .iter()
        .map(|(file, line, text)| Excerpt {
            worktree: worktree.to_string(),
            worktree_label: label.to_string(),
            kind: ExcerptKind::ContentMatch,
            file: file.clone(),
            line: Some(*line),
            text: text.clone(),
            detail: String::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::CiState;

    fn run(name: &str, state: CiState) -> CiRun {
        CiRun {
            id: format!("id-{name}"),
            name: name.to_string(),
            state,
            branch: "b".into(),
            ..Default::default()
        }
    }

    fn ex(label: &str, kind: ExcerptKind, file: &str, text: &str) -> Excerpt {
        Excerpt {
            worktree: format!("/wt/{label}"),
            worktree_label: label.to_string(),
            kind,
            file: file.to_string(),
            line: None,
            text: text.to_string(),
            detail: String::new(),
        }
    }

    #[test]
    fn ci_builder_keeps_only_failures() {
        let runs = vec![
            run("build", CiState::Fail),
            run("test", CiState::Pass),
            run("lint", CiState::Running),
            run("deploy", CiState::Fail),
        ];
        let ex = ci_failure_excerpts("/wt/feat", "feat", &runs);
        assert_eq!(ex.len(), 2);
        assert!(ex.iter().all(|e| e.kind == ExcerptKind::CiFailure));
        let names: Vec<&str> = ex.iter().map(|e| e.text.as_str()).collect();
        assert!(names.contains(&"build") && names.contains(&"deploy"));
        // Empty run name falls back to id; url-less run uses branch as detail.
        assert_eq!(ex[0].detail, "b");
    }

    #[test]
    fn dirty_and_content_builders_map_fields() {
        let dirty = dirty_file_excerpts(
            "/wt/x",
            "x",
            &[("src/a.rs".into(), "M".into()), ("b.rs".into(), "A".into())],
        );
        assert_eq!(dirty.len(), 2);
        assert_eq!(dirty[0].kind, ExcerptKind::DirtyFile);
        assert_eq!(dirty[0].file, "src/a.rs");
        assert_eq!(dirty[0].detail, "M");

        let matches =
            content_match_excerpts("/wt/x", "x", &[("src/a.rs".into(), 42, "TODO fix".into())]);
        assert_eq!(matches[0].kind, ExcerptKind::ContentMatch);
        assert_eq!(matches[0].line, Some(42));
        assert_eq!(matches[0].text, "TODO fix");
    }

    #[test]
    fn sort_is_deterministic_and_grouped() {
        // Deliberately unsorted, two worktrees, mixed kinds.
        let input = vec![
            ex("zeta", ExcerptKind::ContentMatch, "z.rs", "m"),
            ex("alpha", ExcerptKind::DirtyFile, "a.rs", "d"),
            ex("alpha", ExcerptKind::CiFailure, "", "build"),
            ex("zeta", ExcerptKind::CiFailure, "", "test"),
        ];
        let agg = Aggregation::from_excerpts(input.clone());
        let order: Vec<(&str, ExcerptKind)> = agg
            .excerpts()
            .iter()
            .map(|e| (e.worktree_label.as_str(), e.kind))
            .collect();
        // alpha group before zeta; within a group failures precede others.
        assert_eq!(
            order,
            vec![
                ("alpha", ExcerptKind::CiFailure),
                ("alpha", ExcerptKind::DirtyFile),
                ("zeta", ExcerptKind::CiFailure),
                ("zeta", ExcerptKind::ContentMatch),
            ]
        );
        // Same inputs in a different arrival order → identical result (stable).
        let mut shuffled = input;
        shuffled.reverse();
        assert_eq!(Aggregation::from_excerpts(shuffled), agg);
    }

    #[test]
    fn rows_interleave_dividers_and_excerpts() {
        let agg = Aggregation::from_excerpts(vec![
            ex("alpha", ExcerptKind::CiFailure, "", "build"),
            ex("alpha", ExcerptKind::DirtyFile, "a.rs", "d"),
            ex("zeta", ExcerptKind::CiFailure, "", "test"),
        ]);
        let rows = agg.rows();
        assert_eq!(
            rows,
            vec![
                AggRow::Group {
                    label: "alpha".into(),
                    count: 2
                },
                AggRow::Excerpt(0),
                AggRow::Excerpt(1),
                AggRow::Group {
                    label: "zeta".into(),
                    count: 1
                },
                AggRow::Excerpt(2),
            ]
        );
        // Excerpt rows resolve back to their worktree.
        assert_eq!(agg.jump_target(0).unwrap().worktree, "/wt/alpha");
        assert_eq!(agg.jump_target(2).unwrap().worktree, "/wt/zeta");
        assert!(agg.jump_target(3).is_none());
    }

    #[test]
    fn summary_counts_kinds_and_worktrees() {
        let agg = Aggregation::from_excerpts(vec![
            ex("a", ExcerptKind::CiFailure, "", "x"),
            ex("a", ExcerptKind::DirtyFile, "f", "y"),
            ex("b", ExcerptKind::CiFailure, "", "z"),
        ]);
        let s = agg.summary();
        assert_eq!(s.failures, 2);
        assert_eq!(s.dirty, 1);
        assert_eq!(s.matches, 0);
        assert_eq!(s.worktrees, 2);
    }

    #[test]
    fn empty_aggregation_is_empty() {
        let agg = Aggregation::default();
        assert!(agg.is_empty());
        assert_eq!(agg.len(), 0);
        assert!(agg.rows().is_empty());
        assert!(agg.jump_target(0).is_none());
        assert_eq!(agg.summary(), AggSummary::default());
    }
}
