//! Bundled merge-queue agent assets, embedded in the binary and auto-seeded into
//! each worktree's project `.claude/` dir so any Claude-family agent a user runs
//! inside thegn discovers the merge-queue commands without hand-installing
//! anything. The `thegn merge` CLI exposes the same actions; this is the
//! discoverability layer for shell-pane agents.
//!
//! Two kinds ship, matching how Claude Code treats them:
//!   - `.claude/skills/mq/SKILL.md` — the model-discovered overview (`/mq`),
//!   - `.claude/commands/mq-*.md` — explicit user-invoked prompt templates.
//!
//! Sources live under `extensions/` (tracked, so `test/brand-guard.sh` and the
//! tests at the bottom of this file guard them) and are `include_str!`d here.

use std::path::Path;
use thegn_core::config::Config;

/// One bundled agent asset: where it lands under the worktree, and its body.
struct Asset {
    /// Destination path, relative to the worktree root. Forward slashes: `Path`
    /// accepts them on Windows too.
    rel: &'static str,
    body: &'static str,
}

const ASSETS: &[Asset] = &[
    Asset {
        rel: ".claude/skills/mq/SKILL.md",
        body: include_str!("../../../extensions/skills/mq/SKILL.md"),
    },
    Asset {
        rel: ".claude/commands/mq-add.md",
        body: include_str!("../../../extensions/commands/mq-add.md"),
    },
    Asset {
        rel: ".claude/commands/mq-drain.md",
        body: include_str!("../../../extensions/commands/mq-drain.md"),
    },
];

/// Local-ignore patterns (anchored at each worktree's top level) so the seeded
/// assets never show up as untracked changes in `git status`.
///
/// The skill entry stays **directory-shaped** on purpose: builds before the
/// commands existed wrote exactly `.claude/skills/mq/`, and a file-shaped
/// pattern would append a second, redundant line to every repo already seeded.
/// `exclude_pats_cover_every_asset` keeps this list in sync with `ASSETS`.
const EXCLUDE_PATS: &[&str] = &[
    ".claude/skills/mq/",
    ".claude/commands/mq-add.md",
    ".claude/commands/mq-drain.md",
];

/// Seed the bundled assets into a worktree (idempotent overwrite) and locally
/// ignore them. Returns an error only on I/O failure at a write site.
pub fn seed(worktree: &Path) -> std::io::Result<()> {
    for asset in ASSETS {
        let dest = worktree.join(asset.rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(dest, asset.body)?;
    }
    exclude_locally(worktree);
    Ok(())
}

/// Seed every persisted **local** worktree once (best-effort). Covers worktrees
/// created before this build — which is how already-seeded repos pick up newly
/// bundled assets; newly-created ones are seeded at create time. No-op when the
/// merge queue is disabled. Kept here (not `run.rs`) to keep that god-file lean.
pub fn seed_persisted_worktrees(cfg: &Config) {
    // Global on purpose: this seeds a discoverability asset at startup, with no
    // repo in scope. See `Config::merge_queue`.
    if !cfg.merge_queue.enabled {
        return;
    }
    if let Ok(db) = thegn_core::db::Db::open() {
        use thegn_core::store::WorkspaceStore;
        for wt in db.worktrees().unwrap_or_default() {
            if wt.location.is_empty() {
                let _ = seed(std::path::Path::new(&wt.worktree));
            }
        }
    }
}

/// Gated, best-effort seed: only when the merge queue is enabled. The assets are
/// a convenience, never load-bearing — failures (e.g. a read-only canonical
/// tree) must not disrupt worktree creation.
pub fn seed_if_enabled(cfg: &Config, worktree: &Path) {
    if cfg.merge_queue.enabled {
        // best-effort: discoverability aid, not a correctness requirement.
        let _ = seed(worktree);
    }
}

/// Append any missing `EXCLUDE_PATS` to the repo's shared `.git/info/exclude` so
/// the seeded assets are ignored across all worktrees. Same idiom as
/// `worktree::add_checked` uses for `.worktrees/`.
fn exclude_locally(worktree: &Path) {
    let excl = thegn_core::util::git_common_dir(worktree)
        .join("info")
        .join("exclude");
    // A missing file is not a reason to bail: git creates `info/exclude` from a
    // template, but a bare/minimal repo may not have one, and the assets would
    // then sit visible in `git status` forever.
    let contents = std::fs::read_to_string(&excl).unwrap_or_default();
    let missing: Vec<&str> = EXCLUDE_PATS
        .iter()
        .copied()
        .filter(|pat| !contents.lines().any(|l| l.trim() == *pat))
        .collect();
    if missing.is_empty() {
        return;
    }
    if let Some(parent) = excl.parent() {
        // best-effort: cosmetic ignore, never worth failing a seed over.
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&excl)
    {
        // best-effort throughout: a failed write just means the seeded assets
        // show as untracked.
        if !contents.is_empty() && !contents.ends_with('\n') {
            let _ = writeln!(f);
        }
        for pat in missing {
            let _ = writeln!(f, "{pat}");
        }
    }
}

// ── Asset validation ────────────────────────────────────────────────────────
//
// These helpers back the tests below. They live outside `mod tests` only so the
// parsing stays readable; nothing in the runtime path calls them.

/// The `---`-delimited YAML frontmatter of a markdown asset, if well-formed.
#[cfg(test)]
fn frontmatter(body: &str) -> Option<&str> {
    let rest = body.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Top-level `key: value` pairs of a frontmatter block. Deliberately not a YAML
/// parser — these assets are flat by construction, and a real parser would drag
/// a dep in for three files.
#[cfg(test)]
fn fm_get<'a>(fm: &'a str, key: &str) -> Option<&'a str> {
    fm.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// Every command-ish region of a markdown body: fenced code blocks, inline code
/// spans, and `Bash(...)` permission entries.
///
/// Prose is deliberately NOT scanned — "add it to the thegn merge queue" is
/// English, not a command, and would false-positive the CLI check below.
#[cfg(test)]
fn command_regions(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push(line.to_string());
            continue;
        }
        // Inline code spans are the odd-indexed pieces of a backtick split.
        for (i, piece) in line.split('`').enumerate() {
            if i % 2 == 1 {
                out.push(piece.to_string());
            }
        }
    }
    // `allowed-tools: Bash(thegn merge add:*)` — frontmatter, so never inside a
    // fence or a code span.
    let mut rest = body;
    while let Some(start) = rest.find("Bash(") {
        rest = &rest[start + "Bash(".len()..];
        if let Some(end) = rest.find(')') {
            out.push(rest[..end].to_string());
            rest = &rest[end..];
        }
    }
    out
}

/// Subcommand paths a region claims, e.g. `["merge", "add"]` from
/// `thegn merge add --all`. Stops at the first token that is not a bare
/// subcommand word (a flag, a `<placeholder>`, a shell variable).
#[cfg(test)]
fn claimed_paths(region: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let toks: Vec<&str> = region.split_whitespace().collect();
    for (i, tok) in toks.iter().enumerate() {
        // `thegn-core`, `thegnfoo` &c. are not invocations. Only the bare word.
        if *tok != "thegn" {
            continue;
        }
        let mut path = Vec::new();
        for next in &toks[i + 1..] {
            // `Bash(thegn merge add:*)` — the permission glob is not an arg.
            let word = next.trim_end_matches(":*").trim_end_matches(':');
            let bare = !word.is_empty()
                && word.starts_with(|c: char| c.is_ascii_lowercase())
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !bare {
                break;
            }
            path.push(word.to_string());
        }
        if !path.is_empty() {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sz-mq-assets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".git").join("info")).unwrap();
        // A real-ish info/exclude so the append path runs, mirroring a
        // git-initialised repo.
        std::fs::write(
            d.join(".git").join("info").join("exclude"),
            "# git ignores\n",
        )
        .unwrap();
        d
    }

    fn excl_hits(wt: &std::path::Path) -> Vec<(String, usize)> {
        let excl = std::fs::read_to_string(wt.join(".git/info/exclude")).unwrap();
        EXCLUDE_PATS
            .iter()
            .map(|pat| {
                (
                    (*pat).to_string(),
                    excl.lines().filter(|l| l.trim() == *pat).count(),
                )
            })
            .collect()
    }

    #[test]
    fn seed_writes_every_asset_and_is_idempotent() {
        let wt = scratch("idem");
        seed(&wt).unwrap();
        for asset in ASSETS {
            let path = wt.join(asset.rel);
            assert!(path.exists(), "{} not seeded", asset.rel);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), asset.body);
        }
        // The skill still carries the CLI it documents.
        let skill = std::fs::read_to_string(wt.join(".claude/skills/mq/SKILL.md")).unwrap();
        assert!(skill.contains("thegn merge add"));

        // Second seed: still fine, and no exclude line is duplicated.
        seed(&wt).unwrap();
        for (pat, n) in excl_hits(&wt) {
            assert_eq!(n, 1, "{pat} should be excluded exactly once");
        }

        let _ = std::fs::remove_dir_all(&wt);
    }

    /// A repo with no `info/exclude` (or no `info/`) still gets one, rather than
    /// leaving the seeded assets visible in `git status` forever.
    #[test]
    fn seed_creates_a_missing_exclude_file() {
        let wt = scratch("noexcl");
        std::fs::remove_dir_all(wt.join(".git/info")).unwrap();
        seed(&wt).unwrap();
        for (pat, n) in excl_hits(&wt) {
            assert_eq!(n, 1, "{pat} should be excluded exactly once");
        }
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// An exclude file with no trailing newline must not get its last line
    /// glued to our first pattern.
    #[test]
    fn seed_does_not_glue_onto_an_unterminated_exclude() {
        let wt = scratch("noeol");
        std::fs::write(wt.join(".git/info/exclude"), "target/").unwrap();
        seed(&wt).unwrap();
        let excl = std::fs::read_to_string(wt.join(".git/info/exclude")).unwrap();
        assert!(
            excl.lines().any(|l| l.trim() == "target/"),
            "pre-existing line mangled: {excl:?}"
        );
        for (pat, n) in excl_hits(&wt) {
            assert_eq!(n, 1, "{pat} should be excluded exactly once");
        }
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// `ASSETS` and `EXCLUDE_PATS` are two hand-maintained lists; a new asset
    /// with no ignore pattern would show up as untracked in every user's repo.
    #[test]
    fn exclude_pats_cover_every_asset() {
        for asset in ASSETS {
            let covered = EXCLUDE_PATS.iter().any(|pat| {
                if let Some(dir) = pat.strip_suffix('/') {
                    asset.rel.starts_with(&format!("{dir}/"))
                } else {
                    asset.rel == *pat
                }
            });
            assert!(
                covered,
                "{} has no EXCLUDE_PATS entry — it would show as untracked",
                asset.rel
            );
        }
    }

    /// Frontmatter is what makes an asset discoverable at all: a skill with no
    /// `description` is invisible to the model, and a `name:` that disagrees
    /// with its directory loads under a name nobody typed.
    #[test]
    fn every_asset_has_valid_frontmatter() {
        for asset in ASSETS {
            let fm = frontmatter(asset.body)
                .unwrap_or_else(|| panic!("{}: no `---` frontmatter block", asset.rel));

            let desc = fm_get(fm, "description")
                .unwrap_or_else(|| panic!("{}: frontmatter has no `description`", asset.rel));
            assert!(!desc.is_empty(), "{}: empty `description`", asset.rel);

            let stem = asset
                .rel
                .rsplit('/')
                .next()
                .and_then(|f| f.strip_suffix(".md"))
                .unwrap_or_else(|| panic!("{}: not a .md asset", asset.rel));

            // A skill's identity is its directory; a command's is its filename.
            let expected = if stem == "SKILL" {
                asset.rel.trim_end_matches("/SKILL.md").rsplit('/').next()
            } else {
                Some(stem)
            }
            .unwrap();

            match fm_get(fm, "name") {
                Some(name) => assert_eq!(
                    name, expected,
                    "{}: frontmatter `name` disagrees with its path",
                    asset.rel
                ),
                // Commands take their name from the filename, so `name:` is
                // optional there. A skill without one has no identity at all.
                None => assert_ne!(stem, "SKILL", "{}: a skill must declare `name`", asset.rel),
            }
        }
    }

    /// Every `thegn …` invocation the assets tell an agent to run must resolve
    /// against the real clap tree.
    ///
    /// This is the check that would have caught the **pre-rename binary name**
    /// these commands invoked for months while they lived outside the repo —
    /// and unlike `test/brand-guard.sh`, which only knows the one old brand, it
    /// keeps catching ordinary renames and typos (`merge remove` for
    /// `merge rm`) too.
    #[test]
    fn asset_cli_invocations_resolve_against_clap() {
        use clap::CommandFactory;
        let root = crate::Cli::command();

        let mut bad: Vec<String> = Vec::new();
        for asset in ASSETS {
            for region in command_regions(asset.body) {
                for path in claimed_paths(&region) {
                    let mut cur = &root;
                    for (i, tok) in path.iter().enumerate() {
                        match cur.find_subcommand(tok) {
                            Some(sub) => cur = sub,
                            None => {
                                // Once we reach a leaf, the rest are positional
                                // args (`thegn merge rm <worktree>`), not typos.
                                if cur.get_subcommands().next().is_some() {
                                    bad.push(format!(
                                        "{}: `thegn {}` — `{}` is not a subcommand of `{}`",
                                        asset.rel,
                                        path[..=i].join(" "),
                                        tok,
                                        cur.get_name()
                                    ));
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "bundled assets name commands that do not exist:\n  {}",
            bad.join("\n  ")
        );
    }

    /// Guard the guard: the scanner must actually reject a bogus subcommand,
    /// and must not trip over the prose/placeholder shapes the assets use.
    #[test]
    fn claimed_paths_scans_commands_not_prose() {
        assert_eq!(
            claimed_paths("thegn merge add --all"),
            vec![vec!["merge".to_string(), "add".to_string()]]
        );
        // Permission globs.
        assert_eq!(
            claimed_paths("thegn merge list:*"),
            vec![vec!["merge".to_string(), "list".to_string()]]
        );
        // Placeholders end the path.
        assert_eq!(
            claimed_paths("thegn merge rm <worktree>"),
            vec![vec!["merge".to_string(), "rm".to_string()]]
        );
        // A bare mention claims nothing.
        assert!(claimed_paths("thegn").is_empty());
        // Not an invocation.
        assert!(claimed_paths("thegn-core merge add").is_empty());

        // Prose is out of scope entirely — only code regions are scanned.
        assert!(command_regions("add it to the thegn merge queue").is_empty());
        assert_eq!(
            command_regions("Run `thegn merge drain --json` now."),
            vec!["thegn merge drain --json".to_string()]
        );
    }
}
