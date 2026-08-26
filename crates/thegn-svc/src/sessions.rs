//! Coding-agent session-store discovery — the `agent.sessions` capability's I/O
//! edge, and the **one generic walker** driven by each harness's
//! [`thegn_core::harness::SessionLayout`]. The layout rules (which subdir, which
//! filenames) and the credential-free summary parsers live in the harness seam;
//! this walks the filesystem and assembles [`SessionRecord`]s.
//!
//! Contract (see the change's spec): a **bounded, read-on-demand** scan that
//! runs off the event loop, never spawns the harness or spends tokens, and never
//! returns credential material or transcript bodies — only ids, mtimes, the
//! recorded worktree, and a truncated one-line summary. It never errors: an
//! unreadable home simply contributes nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thegn_core::config::Config;
use thegn_core::harness::{self, Harness, SessionLayout, SessionRecord};
use thegn_core::usage;

/// Cap on session files assembled in one discovery pass. A long-lived machine
/// accumulates thousands; the newest N describe what a caller is asking about,
/// and the bound keeps the scan cheap — the same discipline as the token rollup.
const MAX_SESSIONS: usize = 500;

/// Narrow a discovery pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionFilter<'a> {
    /// Only sessions whose recorded working dir equals this worktree.
    pub worktree: Option<&'a str>,
    /// Only sessions from this harness id.
    pub harness: Option<&'a str>,
}

/// Discover sessions across every `SESSIONS`-capable harness's local store.
///
/// `known_worktrees` is the set of thegn-tracked worktree paths, used only to
/// set each record's `unlinked` flag — sessions in worktrees thegn does not
/// track are still listed. Newest-first, bounded by [`MAX_SESSIONS`].
pub fn discover(
    cfg: &Config,
    filter: &SessionFilter,
    known_worktrees: &HashSet<String>,
) -> Vec<SessionRecord> {
    // Reuse the usage layer's tested credential-home enumeration (default homes,
    // profile-root scan, configured accounts — `crate::usage`), then keep only
    // the homes whose harness advertises a session store. Dedup lives in
    // `thegn_core::usage::discover_homes`.
    let homes = usage::discover_homes(&crate::usage::candidate_homes(&cfg.usage, &[]));

    // Collect (mtime, path, harness, layout) across every store first, so the
    // MAX_SESSIONS cap bounds the *reads* to the newest files.
    let mut found: Vec<(SystemTime, PathBuf, &'static dyn Harness, SessionLayout)> = Vec::new();
    for home in &homes {
        let Some(h) = harness::harness(&home.provider) else {
            continue;
        };
        if let Some(want) = filter.harness
            && want != h.id()
        {
            continue;
        }
        let Some(layout) = h.session_layout() else {
            continue; // not a SESSIONS harness
        };
        let store = home.dir.join(layout.store_subdir);
        let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
        collect_session_files(&store, &layout, &mut files);
        for (mtime, path) in files {
            found.push((mtime, path, h, layout));
        }
    }
    // Newest first so the cap keeps the most relevant sessions.
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.truncate(MAX_SESSIONS);

    let mut out: Vec<SessionRecord> = Vec::new();
    for (mtime, path, h, layout) in found {
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| layout.session_id(n))
        else {
            continue;
        };
        // A summary parse is best-effort — a session with an unreadable or
        // still-being-written head is listed with an empty summary, not dropped.
        let summary = std::fs::read(&path)
            .ok()
            .and_then(|bytes| h.parse_session_summary(&bytes))
            .unwrap_or_default();
        let worktree = summary.cwd.filter(|c| !c.is_empty());
        if let Some(want) = filter.worktree
            && worktree.as_deref() != Some(want)
        {
            continue;
        }
        // Unlinked when the worktree is unknown to thegn — or unattributable
        // (no recorded cwd), which cannot be linked either.
        let unlinked = worktree
            .as_deref()
            .map_or(true, |w| !known_worktrees.contains(w));
        out.push(SessionRecord {
            harness: h.id().to_string(),
            id,
            worktree,
            mtime: mtime
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            summary: summary.summary,
            unlinked,
        });
    }
    out
}

/// Recursively collect a layout's transcript files (with mtimes) under `store`.
/// Absent store is the normal case (a harness never launched here), not an error.
fn collect_session_files(
    store: &Path,
    layout: &SessionLayout,
    out: &mut Vec<(SystemTime, PathBuf)>,
) {
    let mut stack = vec![store.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !layout.matches(name) {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            out.push((mtime, path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::UsageConfig;

    /// A unique scratch dir (clock + thread id, like the usage tests).
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tg-sessions-{tag}-{}-{:?}",
            thegn_core::util::now(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A config whose only credential home is a configured `[[usage.accounts]]`
    /// entry pointing at `dir` for `provider` — hermetic, no env/profile scan.
    fn cfg_for(provider: &str, dir: &Path) -> Config {
        Config {
            usage: UsageConfig {
                enabled: true,
                allow_network: false,
                discover_profiles: false,
                profile_roots: Vec::new(),
                providers: vec![provider.to_string()],
                accounts: vec![thegn_core::usage::UsageAccount {
                    name: "t".into(),
                    provider: provider.to_string(),
                    dir: dir.display().to_string(),
                    enabled: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn discovers_claude_sessions_with_summary_and_unlinked_flag() {
        let home = tmpdir("claude-home");
        let proj = home.join("projects").join("home-u-code-thegn");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("0c1f-uuid.jsonl"),
            concat!(
                r#"{"type":"user","cwd":"/home/u/code/thegn","message":{"role":"user","content":"Fix the bug"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":"ok"}}"#,
            ),
        )
        .unwrap();

        let cfg = cfg_for("claude", &home);
        // Worktree unknown → unlinked.
        let recs = discover(&cfg, &SessionFilter::default(), &HashSet::new());
        assert_eq!(recs.len(), 1, "{recs:?}");
        let r = &recs[0];
        assert_eq!(r.harness, "claude");
        assert_eq!(r.id, "0c1f-uuid");
        assert_eq!(r.worktree.as_deref(), Some("/home/u/code/thegn"));
        assert_eq!(r.summary, "Fix the bug");
        assert!(r.unlinked, "an untracked worktree is flagged unlinked");

        // Worktree known → linked.
        let known: HashSet<String> = ["/home/u/code/thegn".to_string()].into_iter().collect();
        let recs = discover(&cfg, &SessionFilter::default(), &known);
        assert!(!recs[0].unlinked);

        // Harness filter excludes it.
        let recs = discover(
            &cfg,
            &SessionFilter {
                harness: Some("codex"),
                ..Default::default()
            },
            &HashSet::new(),
        );
        assert!(recs.is_empty());

        // Worktree filter selects it, and a non-matching one excludes it.
        let recs = discover(
            &cfg,
            &SessionFilter {
                worktree: Some("/home/u/code/thegn"),
                ..Default::default()
            },
            &HashSet::new(),
        );
        assert_eq!(recs.len(), 1);
        let recs = discover(
            &cfg,
            &SessionFilter {
                worktree: Some("/elsewhere"),
                ..Default::default()
            },
            &HashSet::new(),
        );
        assert!(recs.is_empty());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn discovers_codex_rollouts_and_ignores_non_rollout_files() {
        let home = tmpdir("codex-home");
        let day = home.join("sessions").join("2026").join("08");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-2026-08-25T10-00-00-abc.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/srv/app"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Add a flag"}}"#,
            ),
        )
        .unwrap();
        // A non-rollout file in the same store must be ignored.
        std::fs::write(day.join("history.jsonl"), b"{}").unwrap();

        let cfg = cfg_for("codex", &home);
        let recs = discover(&cfg, &SessionFilter::default(), &HashSet::new());
        assert_eq!(recs.len(), 1, "only the rollout is a session: {recs:?}");
        assert_eq!(recs[0].harness, "codex");
        assert_eq!(recs[0].id, "rollout-2026-08-25T10-00-00-abc");
        assert_eq!(recs[0].worktree.as_deref(), Some("/srv/app"));
        assert_eq!(recs[0].summary, "Add a flag");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn absent_store_yields_nothing_and_never_errors() {
        let home = tmpdir("empty-home");
        let cfg = cfg_for("claude", &home);
        assert!(discover(&cfg, &SessionFilter::default(), &HashSet::new()).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}
