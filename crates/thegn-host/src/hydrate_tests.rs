use super::*;
use crate::hydrate_tuning::DEFAULT_MODEL_REFRESH_MS;
use crate::session::{GroupKind, Session, WorktreeGroup};

#[test]
fn pr_clean_decision_only_acts_on_definitive_states() {
    // Both policies enabled: cleaning is still gated on a DEFINITIVE state.
    let cfg = thegn_core::config::DiskConfig {
        auto_clean_on_merge: true,
        clean_on_pr_closed: true,
        ..Default::default()
    };
    assert_eq!(pr_clean_decision(Some("MERGED"), &cfg), (true, true));
    assert_eq!(pr_clean_decision(Some("CLOSED"), &cfg), (false, true));
    // A still-open PR must never clean.
    assert_eq!(pr_clean_decision(Some("OPEN"), &cfg), (false, false));
    // An unresolvable state (gh/network error → None) must never clean — the
    // regression: this used to be treated as "closed" and delete artifacts.
    assert_eq!(pr_clean_decision(None, &cfg), (false, false));
    assert_eq!(pr_clean_decision(Some("weird"), &cfg), (false, false));

    // Policies are honored when disabled.
    let off = thegn_core::config::DiskConfig {
        auto_clean_on_merge: false,
        clean_on_pr_closed: false,
        ..Default::default()
    };
    assert_eq!(pr_clean_decision(Some("MERGED"), &off), (true, false));
    assert_eq!(pr_clean_decision(Some("CLOSED"), &off), (false, false));
}

fn one_tab_session() -> Session {
    Session {
        id: "s1".into(),
        worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
        active: 0,
    }
}

fn five_worktree_session(active: usize) -> Session {
    Session {
        id: "s1".into(),
        worktrees: (0..5)
            .map(|i| {
                WorktreeGroup::new(
                    format!("app/wt{i}"),
                    if i == 0 {
                        GroupKind::Home
                    } else {
                        GroupKind::Branch
                    },
                    format!("/tmp/app-wt{i}"),
                )
            })
            .collect(),
        active,
    }
}

#[test]
fn neighbor_paths_follow_sidebar_display_order_not_session_order() {
    // Sidebar shows the groups shuffled (pins/sort): 3, 1, 4, 0, 2.
    // Active = 4 sits between 1 (above) and 0 (below) IN DISPLAY ORDER —
    // its session-index neighbors (3, 0) would warm the wrong worktree.
    let session = five_worktree_session(4);
    let order = [3usize, 1, 4, 0, 2];
    let got = neighbor_worktree_paths(&session, &order);
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/tmp/app-wt1"),
            std::path::PathBuf::from("/tmp/app-wt0"),
        ]
    );
}

#[test]
fn neighbor_paths_wrap_at_the_ends() {
    // Active first in display order: "previous" wraps to the last row.
    let session = five_worktree_session(3);
    let order = [3usize, 1, 4, 0, 2];
    let got = neighbor_worktree_paths(&session, &order);
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/tmp/app-wt2"),
            std::path::PathBuf::from("/tmp/app-wt1"),
        ]
    );
}

#[test]
fn neighbor_paths_fall_back_to_session_order_when_active_hidden() {
    // Active group filtered out of the sidebar → session ±1 fallback.
    let session = five_worktree_session(2);
    let order = [3usize, 0];
    let got = neighbor_worktree_paths(&session, &order);
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/tmp/app-wt1"),
            std::path::PathBuf::from("/tmp/app-wt3"),
        ]
    );
}

#[test]
fn neighbor_paths_single_visible_worktree_warms_nothing() {
    let session = five_worktree_session(2);
    let got = neighbor_worktree_paths(&session, &[2usize]);
    assert!(got.is_empty());
}

#[test]
fn glyph_rescan_tiering() {
    let ttl = Duration::from_secs(5);
    // The active worktree always rescans, regardless of cache freshness.
    assert!(should_rescan_glyphs(true, Some(Duration::ZERO), ttl));
    assert!(should_rescan_glyphs(true, None, ttl));
    // A background worktree with no cached row must scan once to populate.
    assert!(should_rescan_glyphs(false, None, ttl));
    // A background worktree with a fresh cached row is served from cache.
    assert!(!should_rescan_glyphs(
        false,
        Some(Duration::from_secs(2)),
        ttl
    ));
    // ...and rescans once the cached row ages past the TTL.
    assert!(should_rescan_glyphs(
        false,
        Some(Duration::from_secs(6)),
        ttl
    ));
    // TTL of 0 (the env opt-out) reverts to always-rescan for background too.
    assert!(should_rescan_glyphs(
        false,
        Some(Duration::from_millis(1)),
        Duration::ZERO
    ));
}

#[test]
fn glyph_scan_clean_read_updates() {
    // A fully successful read produces the scanned values and is `clean` so
    // the caller updates the cache.
    let (row, clean) = merge_glyph_scan(
        None,
        Ok(true),
        Ok(Some((4, 1))),
        Ok(Some("feat".into())),
        "/repo".into(),
        Ok((42, 7)),
        Ok(Some((310, 84))),
    );
    assert_eq!(
        row,
        (
            true,
            4,
            1,
            Some("feat".into()),
            "/repo".into(),
            42,
            7,
            Some((310, 84))
        )
    );
    assert!(clean);
}

#[test]
fn glyph_scan_no_upstream_is_zero_not_error() {
    // `Ok(None)` from ahead_behind is the genuine "no upstream" state: zero
    // arrows, and still a clean read. `Ok(None)` from branch_diff is the genuine
    // "no base" state.
    let prior: GlyphRow = (
        true,
        4,
        1,
        Some("feat".into()),
        "/repo".into(),
        1,
        1,
        Some((2, 2)),
    );
    let (row, clean) = merge_glyph_scan(
        Some(&prior),
        Ok(false),
        Ok(None),
        Ok(Some("feat".into())),
        "/repo".into(),
        Ok((0, 0)),
        Ok(None),
    );
    assert_eq!(
        row,
        (false, 0, 0, Some("feat".into()), "/repo".into(), 0, 0, None)
    );
    assert!(clean);
}

#[test]
fn glyph_scan_transient_error_reuses_prior() {
    // A transient gix error on every read must reuse the prior row, not
    // collapse to zero/clean, and the row is NOT clean (cache untouched).
    let prior: GlyphRow = (
        true,
        4,
        1,
        Some("feat".into()),
        "/repo".into(),
        42,
        7,
        Some((310, 84)),
    );
    let (row, clean) = merge_glyph_scan(
        Some(&prior),
        Err(()),
        Err(()),
        Err(()),
        "/repo".into(),
        Err(()),
        Err(()),
    );
    assert_eq!(row, prior);
    assert!(!clean);
}

#[test]
fn glyph_scan_partial_error_keeps_only_failed_field() {
    // ahead_behind errors (reuse prior counts) while dirty succeeds (fresh).
    let prior: GlyphRow = (
        true,
        4,
        1,
        Some("feat".into()),
        "/repo".into(),
        42,
        7,
        Some((310, 84)),
    );
    let (row, clean) = merge_glyph_scan(
        Some(&prior),
        Ok(false),
        Err(()),
        Ok(Some("feat".into())),
        "/repo".into(),
        Ok((5, 2)),
        Err(()),
    );
    assert_eq!(
        row,
        (
            false,
            4,
            1,
            Some("feat".into()),
            "/repo".into(),
            5,
            2,
            Some((310, 84))
        )
    );
    assert!(!clean);
}

#[test]
fn glyph_scan_error_without_prior_falls_back_to_defaults() {
    // First-ever scan that errors has no prior to reuse: best-effort zeros,
    // and not clean so it won't be cached.
    let (row, clean) = merge_glyph_scan(
        None,
        Err(()),
        Err(()),
        Err(()),
        "/repo".into(),
        Err(()),
        Err(()),
    );
    assert_eq!(row, (false, 0, 0, None, "/repo".into(), 0, 0, None));
    assert!(!clean);
}

#[test]
fn initial_model_is_cheap_and_marks_hydration_pending() {
    let session = one_tab_session();
    let model = build_initial_model(&session, None);
    assert_eq!(model.worktree, "app/home");
    assert_eq!(model.tabs, vec!["1".to_string()]);
    assert_eq!(model.active_tab, 0);
    // The cheap initial model carries no derived rows yet (the event loop
    // builds them once view state is loaded).
    assert!(model.sidebar_rows.is_empty());
    assert!(model.panel.branch == "app/home");
    assert!(model.status.contains("Starting thegn"));
}

/// Workspace tuple: (slug, display, kind, repo_path).
fn ws(slug: &str, path: &str) -> (String, String, String, String) {
    (
        slug.to_string(),
        slug.to_uppercase(),
        "repo".to_string(),
        path.to_string(),
    )
}

#[test]
fn merge_keeps_db_order_and_appends_unknown_live_at_end() {
    let merged = merge_workspace_lists(
        vec![ws("alpha", "/r/alpha"), ws("beta", "/r/beta")],
        vec![ws("beta", ""), ws("gamma", "")],
    );
    let slugs: Vec<_> = merged.iter().map(|(s, _, _, _)| s.as_str()).collect();
    assert_eq!(slugs, vec!["alpha", "beta", "gamma"]);
    assert_eq!(merged[1].3, "/r/beta", "DB entry wins over live fallback");
}

#[test]
fn merge_drops_stale_live_fallback_entries() {
    // "old" is a live fallback (empty path) from a workspace we already
    // switched away from: it must not survive a refresh that no longer
    // lists it as live.
    let merged = merge_workspace_lists(
        vec![ws("alpha", "/r/alpha"), ws("old", "")],
        vec![ws("alpha", "")],
    );
    let slugs: Vec<_> = merged.iter().map(|(s, _, _, _)| s.as_str()).collect();
    assert_eq!(slugs, vec!["alpha"]);
}

#[test]
fn merge_is_idempotent_and_never_duplicates_by_slug() {
    let db_backed = vec![ws("alpha", "/r/alpha")];
    let live = vec![ws("alpha", ""), ws("new", "")];
    let once = merge_workspace_lists(db_backed, live.clone());
    let twice = merge_workspace_lists(once.clone(), live);
    assert_eq!(once, twice);
    assert_eq!(twice.len(), 2);
}

#[test]
fn workspace_list_with_db_lists_current_workspace_once() {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "tg-hydrate-test-{}-{}/db.sqlite",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(p.parent().unwrap());
    let db = thegn_core::db::Db::open_at(&p).unwrap();

    // A mixed-case repo registered in the DB, with its live home group
    // named by the canonical slug (as the host now creates it).
    db.put_workspace("/tmp/WASHU", "WASHU", "repo").unwrap();
    let slug = thegn_core::repo::repo_slug_with(&db, std::path::Path::new("/tmp/WASHU"));
    let session = Session {
        id: "/tmp/WASHU".into(),
        worktrees: vec![WorktreeGroup::new(
            thegn_core::repo::home_tab(&slug),
            GroupKind::Home,
            "/tmp/WASHU",
        )],
        active: 0,
    };

    let list = workspace_list(&session, Some(&db));
    assert_eq!(list.len(), 1, "live + DB entries collapse to one: {list:?}");
    assert_eq!(list[0].0, "washu");
    assert_eq!(
        list[0].3, "/tmp/WASHU",
        "the DB-backed entry (with path) wins"
    );
}

#[test]
fn ticker_pr_cadence_is_a_multiple_of_the_model_cadence() {
    // The ticker emits `RefreshKind::Pr` only from inside the `model_every`
    // block, so PR auto-refresh silently stops unless model_every divides
    // pr_every. Lock that for the shipped defaults.
    let base_ms = 500u64;
    assert_eq!(
        DEFAULT_MODEL_REFRESH_MS % base_ms,
        0,
        "must align to base tick"
    );
    let model_every = (DEFAULT_MODEL_REFRESH_MS / base_ms).max(1);
    let pr_every = PR_REFRESH_INTERVAL.as_millis() as u64 / base_ms;
    assert_eq!(
        pr_every % model_every,
        0,
        "pr_every={pr_every} not a multiple of model_every={model_every}"
    );
}

#[test]
fn load_or_seed_session_registers_bootstrap_workspace() {
    // The bootstrap workspace must land in the `workspaces` table: without
    // a row it exists only as a live fallback in `workspace_list` and
    // vanishes from the sidebar after the first switch away.
    let state_home =
        std::env::temp_dir().join(format!("tg-hydrate-bootstrap-{}-state", std::process::id()));
    let ws_dir =
        std::env::temp_dir().join(format!("tg-hydrate-bootstrap-{}-ws", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_home);
    let _ = std::fs::remove_dir_all(&ws_dir);
    std::fs::create_dir_all(state_home.join("thegn")).unwrap();
    std::fs::create_dir_all(&ws_dir).unwrap();
    let ws_str = ws_dir.to_string_lossy().into_owned();

    // Pin THEGN_SESSION so resolution is deterministic even when the
    // test itself runs inside a live thegn.
    let _env = crate::testenv::EnvVarGuard::set(&[
        ("XDG_STATE_HOME", state_home.to_str().unwrap()),
        ("THEGN_SESSION", &ws_str),
    ]);
    let (session, seeded) = load_or_seed_session(&ws_dir, &Default::default());

    assert!(seeded);
    assert_eq!(session.id, ws_str);
    let db = thegn_core::db::Db::open_at(&state_home.join("thegn/thegn.db")).unwrap();
    let rows = db.workspaces().unwrap();
    let row = rows
        .iter()
        .find(|w| w.repo_path == ws_str)
        .expect("bootstrap workspace registered in the workspaces table");
    assert_eq!(row.kind, "dir", "a plain dir bootstraps as a dir workspace");

    drop(_env);
    let _ = std::fs::remove_dir_all(&state_home);
    let _ = std::fs::remove_dir_all(&ws_dir);
}

#[test]
fn load_or_seed_session_does_not_resurrect_tombstoned_workspace() {
    // Regression: removing a workspace keeps its home checkout on disk (git
    // is truth), so a cold start that resolves to that directory must NOT
    // re-register it — the removal tombstone makes "remove workspace" stick.
    let state_home =
        std::env::temp_dir().join(format!("tg-hydrate-tombstone-{}-state", std::process::id()));
    let ws_dir =
        std::env::temp_dir().join(format!("tg-hydrate-tombstone-{}-ws", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_home);
    let _ = std::fs::remove_dir_all(&ws_dir);
    std::fs::create_dir_all(state_home.join("thegn")).unwrap();
    std::fs::create_dir_all(&ws_dir).unwrap();
    let ws_str = ws_dir.to_string_lossy().into_owned();

    // Pre-tombstone the directory in the very DB load_or_seed_session opens
    // (selected by XDG_STATE_HOME), simulating a prior "remove workspace".
    {
        let db = thegn_core::db::Db::open_at(&state_home.join("thegn/thegn.db")).unwrap();
        db.tombstone_workspace(&ws_str).unwrap();
    }

    // Pin THEGN_SESSION to the tombstoned dir so resolution is deterministic
    // (and exercises the guard) regardless of the test runner's cwd.
    let _env = crate::testenv::EnvVarGuard::set(&[
        ("XDG_STATE_HOME", state_home.to_str().unwrap()),
        ("THEGN_SESSION", &ws_str),
    ]);
    let (session, _seeded) = load_or_seed_session(&ws_dir, &Default::default());
    // It still runs transiently in the directory (a live fallback)…
    assert_eq!(session.id, ws_str);

    let db = thegn_core::db::Db::open_at(&state_home.join("thegn/thegn.db")).unwrap();
    // …but must not be re-registered in the sidebar or re-pinned active.
    assert!(
        !db.workspaces()
            .unwrap()
            .iter()
            .any(|w| w.repo_path == ws_str),
        "tombstoned workspace must not be re-registered"
    );
    assert_eq!(
        db.active_workspace().unwrap(),
        None,
        "tombstoned workspace must not re-pin itself active"
    );

    drop(_env);
    let _ = std::fs::remove_dir_all(&state_home);
    let _ = std::fs::remove_dir_all(&ws_dir);
}

#[test]
fn bootstrap_workspace_survives_switch_in_workspace_list() {
    // End-to-end regression for the disappearing-original-workspace bug:
    // bootstrap, switch to a second workspace, and the original must still
    // be listed (DB-backed, non-empty path) — not dropped as a stale live
    // fallback by merge_workspace_lists.
    let state_home =
        std::env::temp_dir().join(format!("tg-hydrate-survive-{}-state", std::process::id()));
    let ws_a = std::env::temp_dir().join(format!("tg-hydrate-survive-{}-a", std::process::id()));
    let ws_b = std::env::temp_dir().join(format!("tg-hydrate-survive-{}-b", std::process::id()));
    for d in [&state_home, &ws_a, &ws_b] {
        let _ = std::fs::remove_dir_all(d);
    }
    std::fs::create_dir_all(state_home.join("thegn")).unwrap();
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let a_str = ws_a.to_string_lossy().into_owned();
    let b_str = ws_b.to_string_lossy().into_owned();

    let _env = crate::testenv::EnvVarGuard::set(&[
        ("XDG_STATE_HOME", state_home.to_str().unwrap()),
        ("THEGN_SESSION", &a_str),
    ]);
    let (mut session, _) = load_or_seed_session(&ws_a, &Default::default());
    let db = thegn_core::db::Db::open_at(&state_home.join("thegn/thegn.db")).unwrap();
    session.switch_to_workspace(&b_str, &db).unwrap();

    let list = workspace_list(&session, Some(&db));
    let a_slug = thegn_core::repo::repo_slug_with(&db, &ws_a);
    let entry = list
        .iter()
        .find(|(slug, _, _, _)| *slug == a_slug)
        .expect("original workspace still listed after switching away");
    assert_eq!(
        entry.3, a_str,
        "original workspace is DB-backed (non-empty path), not a live fallback"
    );

    drop(_env);
    for d in [&state_home, &ws_a, &ws_b] {
        let _ = std::fs::remove_dir_all(d);
    }
}

// --- audit fixes -------------------------------------------------------

#[test]
fn glyph_persist_entry_serializes_only_the_row() {
    // The DB write moved out of the glyph_cache mutex; this helper is what the
    // loop-off write path serializes. It must round-trip the row verbatim.
    let row: GlyphRow = (
        true,
        3,
        1,
        Some("feature".into()),
        "/repo".into(),
        12,
        3,
        Some((99, 4)),
    );
    let (path, json) = glyph_persist_entry("/repo/wt", &row);
    assert_eq!(path, "/repo/wt");
    let back: GlyphRow = serde_json::from_str(&json).unwrap();
    assert_eq!(back, row);
}

#[test]
fn needs_fallback_send_only_on_non_normal_exit() {
    // Normal exit (body ran + sent a model) must NOT re-send a fallback...
    let normal: std::thread::Result<Option<()>> = Ok(Some(()));
    assert!(!needs_fallback_send(&normal));
    // ...but a handled early return (Db::open failed → Ok(None)) MUST, or the
    // loop's inflight_hydration_gen gate strands forever.
    let db_fail: std::thread::Result<Option<()>> = Ok(None);
    assert!(needs_fallback_send(&db_fail));
    // ...and a caught panic (Err) MUST likewise release the gate.
    let panicked: std::thread::Result<Option<()>> =
        Err(Box::new("boom") as Box<dyn std::any::Any + Send>);
    assert!(needs_fallback_send(&panicked));
}

#[test]
fn pr_state_definitive_gates_cache_writes() {
    use thegn_core::github::PanelState;
    // Definitive answers are cacheable.
    assert!(pr_state_is_definitive(&PanelState::NoPr));
    assert!(pr_state_is_definitive(&PanelState::NoGh));
    assert!(pr_state_is_definitive(&PanelState::NotAuthenticated));
    // Transient failures must NOT overwrite a good cached PrPanel.
    assert!(!pr_state_is_definitive(&PanelState::Offline));
    assert!(!pr_state_is_definitive(&PanelState::RateLimited));
    assert!(!pr_state_is_definitive(&PanelState::Error {
        message: "dns failure".into()
    }));
}

#[test]
fn plan_log_scan_covers_rotation_append_and_idle() {
    // First scan of the process (prev_len == 0): read everything.
    assert_eq!(plan_log_scan(0, 500), LogScanPlan::FromStart);
    // Unchanged length: nothing new — reuse the running total (no re-read).
    assert_eq!(plan_log_scan(500, 500), LogScanPlan::Unchanged);
    // Grew: scan ONLY the appended suffix, not the whole file (the fix).
    assert_eq!(plan_log_scan(500, 900), LogScanPlan::Append { offset: 500 });
    // Shrank (log rotated/truncated): reset and re-scan from the start.
    assert_eq!(plan_log_scan(900, 100), LogScanPlan::FromStart);
}

#[test]
fn count_error_lines_counts_only_errors() {
    let chunk = "2026-06-05T12:00:00  INFO  thegn::db  ok\n\
             2026-06-05T12:00:01  ERROR thegn::host  boom\n\
             2026-06-05T12:00:02  WARN  thegn::x  slow\n\
             2026-06-05T12:00:03  ERROR thegn::y  bang\n";
    assert_eq!(count_error_lines(chunk.as_bytes()), 2);
    assert_eq!(count_error_lines(b""), 0);
}

#[test]
fn update_log_error_total_is_incremental_and_resets_on_rotation() {
    // A real file exercised through the incremental scanner: the running total
    // must fold in only the appended errors, and reset when the file shrinks.
    // The global state is process-wide, so this test drives it end to end.
    let dir = std::env::temp_dir().join(format!(
        "thegn-logscan-{}-{}",
        std::process::id(),
        now_secs()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("thegn.log");

    // Baseline: reset global state to a known point by pointing at an empty
    // file first (FromStart with 0 bytes → total 0).
    std::fs::write(&path, b"").unwrap();
    let len0 = std::fs::metadata(&path).unwrap().len();
    let base = update_log_error_total(&path, len0);

    // Append two ERRORs.
    let a = "2026-06-05T12:00:01  ERROR thegn::a  one\n\
                 2026-06-05T12:00:02  INFO  thegn::a  fine\n\
                 2026-06-05T12:00:03  ERROR thegn::a  two\n";
    std::fs::write(&path, a.as_bytes()).unwrap();
    let len1 = std::fs::metadata(&path).unwrap().len();
    let t1 = update_log_error_total(&path, len1);
    assert_eq!(t1, base + 2, "both appended errors counted");

    // Idle pass (no growth): total unchanged, and no re-scan of old bytes.
    let t_idle = update_log_error_total(&path, len1);
    assert_eq!(t_idle, t1);

    // Append one more ERROR — only the suffix should be scanned, adding 1.
    let mut full = a.to_string();
    full.push_str("2026-06-05T12:00:04  ERROR thegn::a  three\n");
    std::fs::write(&path, full.as_bytes()).unwrap();
    let len2 = std::fs::metadata(&path).unwrap().len();
    let t2 = update_log_error_total(&path, len2);
    assert_eq!(t2, t1 + 1, "only the appended error is added");

    // Rotation: file shrinks to a single ERROR → total resets to that count.
    let rotated = "2026-06-05T13:00:00  ERROR thegn::a  fresh\n";
    std::fs::write(&path, rotated.as_bytes()).unwrap();
    let len3 = std::fs::metadata(&path).unwrap().len();
    let t3 = update_log_error_total(&path, len3);
    assert_eq!(t3, 1, "rotation resets the running total");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_log_tail_lines_is_bounded_and_snaps_to_line() {
    let dir = std::env::temp_dir().join(format!(
        "thegn-logtail-{}-{}",
        std::process::id(),
        now_secs()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("thegn.log");

    // Build a file larger than the tail window so the read must be bounded.
    let mut content = String::new();
    for i in 0..2000 {
        content.push_str(&format!("2026-06-05T12:00:00  INFO  thegn::x  line {i}\n"));
    }
    std::fs::write(&path, content.as_bytes()).unwrap();
    let len = std::fs::metadata(&path).unwrap().len();

    // A small tail window: we must get the LAST lines, and the very first
    // (partial) line of the window must be dropped so no corrupt row leaks.
    let lines = read_log_tail_lines(&path, len, 4 * 1024);
    assert!(!lines.is_empty());
    assert!(
        lines.len() < 2000,
        "tail is bounded, not the whole file: got {}",
        lines.len()
    );
    // The last parsed line is the last log line.
    assert_eq!(lines.last().unwrap().message, "line 1999");
    // Every parsed row is well-formed (no partial first line).
    assert!(lines.iter().all(|l| l.message.starts_with("line ")));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_placement_worktrees_survive_missing_local_dir() {
    use thegn_core::config::{Config, EnvConfig, PlacementMode};
    let mut cfg = Config::default();
    cfg.env.insert(
        "ageless".into(),
        EnvConfig {
            placement: PlacementMode::Ssh,
            ..Default::default()
        },
    );
    cfg.env.insert(
        "machine0".into(),
        EnvConfig {
            placement: PlacementMode::Provider,
            ..Default::default()
        },
    );
    cfg.env.insert(
        "host".into(),
        EnvConfig {
            placement: PlacementMode::Local,
            ..Default::default()
        },
    );

    // Non-local placements are remote even with an EMPTY location (ssh/k8s
    // never persist one; a provider whose bring-up failed hasn't yet), so the
    // local-dir reconcile must NOT reap them. This is the regression: ssh
    // (`ageless`) + provider (`machine0`) worktrees vanished on create.
    assert!(row_is_remote("", Some("ageless"), &cfg));
    assert!(row_is_remote("", Some("machine0"), &cfg));

    // A local env whose dir is gone IS reapable; so is an unknown/absent env.
    assert!(!row_is_remote("", Some("host"), &cfg));
    assert!(!row_is_remote("", Some("gone-env"), &cfg));
    assert!(!row_is_remote("", None, &cfg));

    // A persisted location always wins, regardless of placement.
    assert!(row_is_remote("{\"path\":\"/x\"}", Some("host"), &cfg));
    assert!(row_is_remote("{\"path\":\"/x\"}", None, &cfg));
}

#[test]
fn inherited_remote_ambient_env_survives_missing_local_dir() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use thegn_core::config::{Config, EnvConfig, PlacementMode};
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "tg-hydrate-ambient-{}-{}/db.sqlite",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(p.parent().unwrap());
    let db = thegn_core::db::Db::open_at(&p).unwrap();

    let mut cfg = Config::default();
    cfg.env.insert(
        "ageless".into(),
        EnvConfig {
            placement: PlacementMode::Ssh,
            ..Default::default()
        },
    );
    cfg.env.insert(
        "host".into(),
        EnvConfig {
            placement: PlacementMode::Local,
            ..Default::default()
        },
    );
    let repo = "/tmp/ambient-ssh-repo";
    let mut cache = std::collections::HashMap::new();

    // The regression: a worktree that CLEANLY INHERITS a remote ambient
    // default (env_name NULL, location empty) must be treated as remote —
    // the wizard only pins an env that differs from the ambient default, so
    // an ssh default env leaves the row's `env_name` NULL. The bare
    // `row_is_remote` sees None and would wrongly reap it.
    cfg.sandbox.default_env = "ageless".into();
    assert!(!row_is_remote("", None, &cfg));
    assert!(row_is_remote_effective(
        &db, &cfg, "", None, repo, &mut cache
    ));

    // A local ambient default leaves an inherited NULL-env row reapable.
    let mut cfg_local = cfg.clone();
    cfg_local.sandbox.default_env = "host".into();
    let mut cache2 = std::collections::HashMap::new();
    assert!(!row_is_remote_effective(
        &db,
        &cfg_local,
        "",
        None,
        repo,
        &mut cache2
    ));

    // An explicitly-pinned non-local env still wins over a local ambient.
    assert!(row_is_remote_effective(
        &db,
        &cfg_local,
        "",
        Some("ageless"),
        repo,
        &mut cache2
    ));
    // A persisted location always wins.
    assert!(row_is_remote_effective(
        &db,
        &cfg_local,
        "{\"path\":\"/x\"}",
        None,
        repo,
        &mut cache2
    ));

    let _ = std::fs::remove_dir_all(p.parent().unwrap());
}

#[test]
fn commit_load_needed_open_follows_ttl_warm_only_cold_miss() {
    let now = thegn_core::util::now();
    let fresh = ("[]".to_string(), now);
    let stale = ("[]".to_string(), now - (COMMIT_CACHE_TTL_SECS + 10));

    // Open section: honour the TTL — refresh when cold or stale, skip when fresh.
    assert!(commit_load_needed(true, None));
    assert!(commit_load_needed(true, Some(&stale)));
    assert!(!commit_load_needed(true, Some(&fresh)));

    // Warm-only (closed summary): refresh on a cold miss ONLY. A present cache is
    // reused even when stale, so the ticker never re-runs `git log` for a section
    // nobody's looking at.
    assert!(commit_load_needed(false, None));
    assert!(!commit_load_needed(false, Some(&stale)));
    assert!(!commit_load_needed(false, Some(&fresh)));
}

#[test]
fn branch_fetch_needed_open_follows_ttl_warm_only_cold_miss() {
    use std::time::Duration;
    let ttl = crate::branch_cache::BRANCH_CACHE_TTL;
    let fresh = Some(Duration::from_millis(0));
    let stale = Some(ttl + Duration::from_secs(1));

    // Nothing wanted → never fetch, regardless of cache state.
    assert!(!branch_fetch_needed(false, true, None, ttl));
    assert!(!branch_fetch_needed(false, false, None, ttl));

    // Open section: TTL refresh — cold or stale fetches, fresh skips.
    assert!(branch_fetch_needed(true, true, None, ttl));
    assert!(branch_fetch_needed(true, true, stale, ttl));
    assert!(!branch_fetch_needed(true, true, fresh, ttl));

    // Warm-only: cold miss ONLY — a cached list is reused even when stale, so the
    // repo-global `branches_full` subprocess never runs on the ticker.
    assert!(branch_fetch_needed(true, false, None, ttl));
    assert!(!branch_fetch_needed(true, false, stale, ttl));
    assert!(!branch_fetch_needed(true, false, fresh, ttl));
}
