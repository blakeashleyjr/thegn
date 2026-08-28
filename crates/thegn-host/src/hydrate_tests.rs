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
    let floor = Duration::from_millis(300);
    // The active worktree scans on a FLOOR, not an exemption: a row younger than
    // the floor is served from cache. This is what stops a watcher storm from
    // spending a git fan-out (including a three-dot diff) per debounce window.
    assert!(!should_rescan_glyphs(
        true,
        Some(Duration::from_millis(50)),
        ttl,
        floor
    ));
    // Past the floor it rescans — well inside the TTL a background row would use.
    assert!(should_rescan_glyphs(
        true,
        Some(Duration::from_millis(400)),
        ttl,
        floor
    ));
    // No cached row: scan, active or not.
    assert!(should_rescan_glyphs(true, None, ttl, floor));
    assert!(should_rescan_glyphs(false, None, ttl, floor));
    // A background worktree with a fresh cached row is served from cache — and
    // its window is the TTL, not the (much shorter) active floor.
    assert!(!should_rescan_glyphs(
        false,
        Some(Duration::from_secs(2)),
        ttl,
        floor
    ));
    // ...and rescans once the cached row ages past the TTL.
    assert!(should_rescan_glyphs(
        false,
        Some(Duration::from_secs(6)),
        ttl,
        floor
    ));
    // TTL of 0 (the env opt-out) reverts to always-rescan for background too.
    assert!(should_rescan_glyphs(
        false,
        Some(Duration::from_millis(1)),
        Duration::ZERO,
        floor
    ));
    // Floor of 0 (`THEGN_ACTIVE_GLYPH_FLOOR_MS=0`) restores the old
    // always-rescan behavior for the active row.
    assert!(should_rescan_glyphs(
        true,
        Some(Duration::from_millis(1)),
        ttl,
        Duration::ZERO
    ));
}

#[test]
fn glyph_keep_set_unions_registry_and_repo_roots() {
    // Repo roots ride along with registered worktrees — a dormant workspace's
    // home row is keyed by its repo root, which has no registry row, and must
    // survive the cache retain across a workspace switch.
    let (set, keep_ok) = glyph_keep_set(
        Some(vec!["/wt/a".into(), String::new(), "/wt/b".into()]),
        Some(vec!["/repo/app".into(), String::new()]),
    );
    assert!(keep_ok);
    assert_eq!(set, vec!["/wt/a", "/wt/b", "/repo/app"]);
}

#[test]
fn glyph_keep_set_distrusts_failed_db_reads() {
    // A transient DB error must not read as "no worktrees" — evicting on it
    // would blank every dormant workspace's glyphs until restart. Either read
    // failing makes the set untrustworthy for eviction; whatever WAS read is
    // still returned (seeding from a partial set only ever adds).
    let (set, keep_ok) = glyph_keep_set(None, Some(vec!["/repo/app".into()]));
    assert!(!keep_ok);
    assert_eq!(set, vec!["/repo/app"]);

    let (set, keep_ok) = glyph_keep_set(Some(vec!["/wt/a".into()]), None);
    assert!(!keep_ok);
    assert_eq!(set, vec!["/wt/a"]);

    let (set, keep_ok) = glyph_keep_set(None, None);
    assert!(!keep_ok);
    assert!(set.is_empty());
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
fn weather_emits_no_slot_when_disabled() {
    // The 0%-idle contract for `[weather]`: with the feature off the config
    // yields no poll interval, the ticker derives no slot count, and the emit
    // guard (`weather_every.is_some_and(…)`) can therefore never fire — not even
    // on `WEATHER_FIRST_SLOT`.
    let off = thegn_core::config_weather::WeatherConfig::default();
    assert_eq!(off.poll_secs(), None, "the shipped default is off");
    assert_eq!(weather_every_slots(off.poll_secs()), None);
    assert_eq!(weather_every_slots(None), None);
    // The guard the ticker actually runs, over the startup slot and a long run.
    let every = weather_every_slots(off.poll_secs());
    for ticks in [0u64, WEATHER_FIRST_SLOT, 1_000, 100_000] {
        assert!(
            !every.is_some_and(|n| ticks == WEATHER_FIRST_SLOT || ticks.is_multiple_of(n)),
            "a disabled `[weather]` must emit no slot at tick {ticks}"
        );
    }
}

#[test]
fn a_stray_zero_interval_is_floored() {
    use thegn_core::config_weather::{MIN_REFRESH_SECS, WeatherConfig};
    // 600s at a 500ms tick.
    let floor_slots = (MIN_REFRESH_SECS * 1000) / 500;
    let spinny = WeatherConfig {
        enabled: true,
        refresh_interval_secs: 0,
        ..Default::default()
    };
    // Floored by the config accessor…
    assert_eq!(spinny.poll_secs(), Some(MIN_REFRESH_SECS));
    // …and again at the one place that loops, so neither alone is load-bearing.
    assert_eq!(weather_every_slots(spinny.poll_secs()), Some(floor_slots));
    assert_eq!(weather_every_slots(Some(0)), Some(floor_slots));
    // The shipped default interval (30 min) is not floored away.
    let default_on = WeatherConfig {
        enabled: true,
        ..Default::default()
    };
    assert_eq!(weather_every_slots(default_on.poll_secs()), Some(3_600));
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
    use thegn_core::forge::model::PanelState;
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

// --- THE-73: only git may condemn a worktree row -----------------------

/// A real git repo at `root` with one linked worktree at `linked` (whose parent
/// must already exist). Both are created with the developer's global gitconfig
/// neutralised — a global `commit.gpgsign = true` otherwise hangs the fixture
/// waiting on a signature it cannot get in a sandboxed run.
///
/// Returns the two paths as git itself resolves them, so the assertions compare
/// against the same strings the porcelain prints even if the temp dir is a
/// symlink.
fn git_repo_with_linked_worktree(
    root: &std::path::Path,
    linked: &std::path::Path,
) -> (String, String) {
    use thegn_core::util::git_cmd;
    std::fs::create_dir_all(root).unwrap();
    std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
    // test code: fixture setup, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    let run = |dir: &std::path::Path, args: &[&str]| {
        assert!(
            git_cmd(dir).args(args).status().unwrap().success(),
            "git -C {dir:?} {args:?}"
        );
    };
    run(root, &["init", "-q", "-b", "main"]);
    run(root, &["config", "user.email", "t@t.t"]);
    run(root, &["config", "user.name", "t"]);
    run(root, &["config", "commit.gpgsign", "false"]);
    run(root, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let linked_arg = linked.to_string_lossy().into_owned();
    run(root, &["worktree", "add", "-q", "-b", "feat", &linked_arg]);

    let porcelain = thegn_core::util::git_out(root, &["worktree", "list", "--porcelain"])
        .expect("porcelain from a freshly built fixture");
    let pairs = thegn_core::util::parse_worktree_branches(&porcelain);
    let of = |branch: &str| {
        pairs
            .iter()
            .find(|(_, b)| b.as_deref() == Some(branch))
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| panic!("git lists a {branch} worktree: {porcelain}"))
    };
    (of("main"), of("feat"))
}

#[test]
fn row_is_git_listed_is_not_a_worktrees_dir_prefix_test() {
    // THE-73 — the local-foreign-dir sibling of the `row_is_remote` guard
    // above. A worktree git still lists is real WHEREVER it sits on disk, so
    // the guard must key on git membership and nothing else. The linked
    // worktree here lives under a second temp dir that shares no prefix with
    // the repo (nor with any plausible `[core] worktrees_dir`), so any
    // containment or path-prefix rule would wrongly condemn it.
    let base = std::env::temp_dir().join(format!("tg-the73-listed-{}", std::process::id()));
    let repo = base.join("repo");
    let far = base.join("somewhere-else-entirely").join("wt");
    let _ = std::fs::remove_dir_all(&base);
    let (root_s, far_s) = git_repo_with_linked_worktree(&repo, &far);

    let mut cache = std::collections::HashMap::new();
    assert!(
        row_is_git_listed(&root_s, &far_s, &mut cache),
        "a git-listed worktree outside every worktrees_dir must survive"
    );
    // The main checkout is listed too, and a trailing slash / doubled separator
    // is absorbed by component equality rather than by string munging.
    assert!(row_is_git_listed(&root_s, &root_s, &mut cache));
    assert!(row_is_git_listed(&root_s, &format!("{far_s}/"), &mut cache));
    assert!(row_is_git_listed(
        &root_s,
        &far_s.replace("/wt", "//wt"),
        &mut cache
    ));

    // The dir going missing does NOT change the verdict: git still lists the
    // worktree (as `prunable`), and only git may condemn the row.
    std::fs::remove_dir_all(&far).unwrap();
    let mut cache2 = std::collections::HashMap::new();
    assert!(
        row_is_git_listed(&root_s, &far_s, &mut cache2),
        "a missing dir is not proof of deletion while git still lists it"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn row_is_git_listed_is_false_for_a_path_git_never_knew() {
    // The guard must still AUTHORISE a real reap, or it would just leak rows.
    let base = std::env::temp_dir().join(format!("tg-the73-unknown-{}", std::process::id()));
    let repo = base.join("repo");
    let far = base.join("elsewhere").join("wt");
    let _ = std::fs::remove_dir_all(&base);
    let (root_s, _far_s) = git_repo_with_linked_worktree(&repo, &far);

    let mut cache = std::collections::HashMap::new();
    let never = repo
        .join("never-was-a-worktree")
        .to_string_lossy()
        .into_owned();
    assert!(!row_is_git_listed(&root_s, &never, &mut cache));
    // Even a path INSIDE the repo tree is not listed — membership, not prefix.
    assert!(!row_is_git_listed(
        &root_s,
        &format!("{root_s}/sub/dir"),
        &mut cache
    ));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn row_is_git_listed_fails_safe_when_the_repo_root_is_unreadable() {
    // We could not prove deletion, so we must not destroy the row — the same
    // posture `row_is_remote` takes for an unknown placement.
    let mut cache = std::collections::HashMap::new();
    assert!(row_is_git_listed("", "/anywhere/at/all", &mut cache));
    let absent = std::env::temp_dir()
        .join(format!("tg-the73-no-such-repo-{}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_dir_all(&absent);
    assert!(row_is_git_listed(&absent, "/anywhere/at/all", &mut cache));
    // An empty worktree path has nothing to match either.
    assert!(row_is_git_listed("/tmp", "", &mut cache));
}

#[test]
fn row_is_git_listed_probes_each_repo_root_once() {
    // N missing rows in one repo must cost ONE subprocess, not N: the reap
    // branch is where the cost lands, so it has to stay bounded.
    let base = std::env::temp_dir().join(format!("tg-the73-memo-{}", std::process::id()));
    let repo = base.join("repo");
    let far = base.join("elsewhere").join("wt");
    let _ = std::fs::remove_dir_all(&base);
    let (root_s, far_s) = git_repo_with_linked_worktree(&repo, &far);

    let mut cache = std::collections::HashMap::new();
    assert!(row_is_git_listed(&root_s, &far_s, &mut cache));
    assert!(!row_is_git_listed(
        &root_s,
        &format!("{root_s}/other"),
        &mut cache
    ));
    assert_eq!(cache.len(), 1, "one probe per repo root per pass");

    // The unaskable root is memoised as `None` too, so a broken root is probed
    // at most once rather than once per condemned row.
    let absent = base.join("gone").to_string_lossy().into_owned();
    assert!(row_is_git_listed(&absent, &far_s, &mut cache));
    assert!(row_is_git_listed(&absent, &far_s, &mut cache));
    assert_eq!(cache.len(), 2);
    assert!(
        cache.get(&absent).is_some_and(|v| v.is_none()),
        "an unaskable root is cached as `None`, not re-probed"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn prune_keeps_a_git_listed_group_whose_dir_is_gone() {
    // The prune that runs before the first frame used to delete a registry row
    // on the strength of one `is_dir` stat. THE-73: a group git still lists
    // survives (row intact), while a group git never knew is still reaped.
    let base = std::env::temp_dir().join(format!("tg-the73-prune-{}", std::process::id()));
    let state_home = base.join("state");
    let repo = base.join("repo");
    let far = base.join("way-over-here").join("wt");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(state_home.join("thegn")).unwrap();
    let (root_s, far_s) = git_repo_with_linked_worktree(&repo, &far);

    let _env =
        crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state_home.to_str().unwrap())]);
    let db = thegn_core::db::Db::open_at(&state_home.join("thegn/thegn.db")).unwrap();

    // A ghost the repo never had a worktree for — the control case.
    let ghost_s = repo.join("ghost").to_string_lossy().into_owned();
    db.put_worktree("repo/feat", &root_s, &far_s, "feat", None, None)
        .unwrap();
    db.put_worktree("repo/ghost", &root_s, &ghost_s, "ghost", None, None)
        .unwrap();

    // Both dirs are absent from disk; only the ghost was never a worktree.
    std::fs::remove_dir_all(&far).unwrap();
    let mut session = Session {
        id: root_s.clone(),
        worktrees: vec![
            WorktreeGroup::new("repo/feat", GroupKind::Branch, far_s.clone()),
            WorktreeGroup::new("repo/ghost", GroupKind::Branch, ghost_s.clone()),
        ],
        active: 0,
    };
    let pruned = prune_stale_worktree_groups(&mut session, &db, "s", &Default::default());

    assert_eq!(pruned, 1, "only the group git never listed is pruned");
    assert_eq!(
        session
            .worktrees
            .iter()
            .map(|g| g.path.as_str())
            .collect::<Vec<_>>(),
        vec![far_s.as_str()],
        "the git-listed group survives its missing dir"
    );
    let rows = db.worktrees().unwrap();
    assert!(
        rows.iter().any(|w| w.worktree == far_s),
        "the git-listed registry row must NOT be deleted"
    );
    assert!(
        !rows.iter().any(|w| w.worktree == ghost_s),
        "the row git never listed is still reaped"
    );

    drop(_env);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn prune_reaps_a_removed_group_whose_registry_row_is_already_gone() {
    // The fail-safe posture must not turn a silent deletion into a silent
    // ACCUMULATION. A worktree removed properly (`thegn wt rm` / `git worktree
    // remove`) while thegn wasn't running leaves a session group with no
    // registry row at all, so the `path → repo_root` map misses and
    // `main_worktree` can't help either (its argument is the dir that's gone).
    // The session's own id is the workspace root for every group in it, so git
    // is still askable — and git says this one is gone.
    let base = std::env::temp_dir().join(format!("tg-the73-prune-rowless-{}", std::process::id()));
    let state_home = base.join("state");
    let repo = base.join("repo");
    let far = base.join("way-over-here").join("wt");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(state_home.join("thegn")).unwrap();
    let (root_s, far_s) = git_repo_with_linked_worktree(&repo, &far);

    // Removed the way thegn removes one: git drops the entry entirely, so the
    // porcelain no longer lists it (unlike a bare `rm -rf`, which leaves a
    // still-listed `prunable` record).
    // test code: fixture teardown, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    let ok = thegn_core::util::git_cmd(&repo)
        .args(["worktree", "remove", "--force", &far_s])
        .status()
        .unwrap()
        .success();
    assert!(ok, "git worktree remove");

    let _env =
        crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state_home.to_str().unwrap())]);
    let db = thegn_core::db::Db::open_at(&state_home.join("thegn/thegn.db")).unwrap();

    // No `put_worktree`: the registry row is already gone, which is the case
    // that used to leave the group un-prunable forever.
    let mut session = Session {
        id: root_s.clone(),
        worktrees: vec![WorktreeGroup::new(
            "repo/feat",
            GroupKind::Branch,
            far_s.clone(),
        )],
        active: 0,
    };
    let pruned = prune_stale_worktree_groups(&mut session, &db, "s", &Default::default());

    assert_eq!(pruned, 1, "git no longer lists it, so the group is reaped");
    assert!(session.worktrees.is_empty());

    drop(_env);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn commit_load_needed_open_follows_ttl_warm_only_cold_miss() {
    let now = thegn_core::util::now();
    let fresh = ("[]".to_string(), now);
    let stale = ("[]".to_string(), now - (COMMIT_CACHE_TTL_SECS + 10));
    let ancient = ("[]".to_string(), now - (COMMIT_SUMMARY_TTL_SECS + 10));

    // Open section: honour the TTL — refresh when cold or stale, skip when fresh.
    assert!(commit_load_needed(true, None));
    assert!(commit_load_needed(true, Some(&stale)));
    assert!(!commit_load_needed(true, Some(&fresh)));

    // Warm-only (closed summary): refresh on a cold miss or once the LONGER
    // summary TTL lapses — the collapsed row's latest-commit line must not be
    // unboundedly stale, but the ticker still never re-runs `git log` per tick.
    assert!(commit_load_needed(false, None));
    assert!(!commit_load_needed(false, Some(&stale)));
    assert!(!commit_load_needed(false, Some(&fresh)));
    assert!(commit_load_needed(false, Some(&ancient)));
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

#[test]
fn pr_linked_diff_emits_once_for_new_prs_only() {
    use std::collections::HashSet;
    use thegn_core::forge::model::PrHeader;
    let pr = |n: u64, head: &str| PrHeader {
        number: n,
        head_ref: head.into(),
        state: "OPEN".into(),
        url: format!("https://x/pull/{n}"),
        is_draft: false,
    };
    let worktrees = vec![
        (
            "/wt/feat".to_string(),
            "feat/x".to_string(),
            vec!["linear:ABC-1".to_string()],
        ),
        // Linked-issue-free worktree: its branch never emits.
        ("/wt/plain".to_string(), "plain".to_string(), vec![]),
    ];
    let hints = vec![(
        "ABC-2".to_string(),
        "abc-2-fix".to_string(),
        "/wt/feat".to_string(),
    )];

    // Already-open PRs are skipped; a new PR on a linked worktree's branch
    // emits, attributed to that worktree.
    let old: HashSet<String> = ["old-branch".to_string()].into();
    let got = pr_linked_notifications(
        &old,
        &[pr(1, "old-branch"), pr(2, "feat/x"), pr(3, "plain")],
        &worktrees,
        &hints,
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, "pr:2");
    assert_eq!(got[0].2, "/wt/feat");
    assert!(got[0].1.contains("linear:ABC-1"), "{}", got[0].1);

    // A branch matching only a linked issue's branch_hint also emits.
    let got = pr_linked_notifications(&old, &[pr(4, "abc-2-fix")], &worktrees, &hints);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, "pr:4");
    assert!(got[0].1.contains("ABC-2"), "{}", got[0].1);

    // Unmatched branches emit nothing.
    assert!(pr_linked_notifications(&old, &[pr(5, "stranger")], &worktrees, &hints).is_empty());
}

#[test]
fn tracker_diff_emits_status_changes_and_blocker_resolved_once() {
    use std::collections::HashSet;
    use thegn_core::issue::{Issue, IssueStatus};
    let issue = |id: &str, num: &str, status: IssueStatus, blocked_by: &[&str]| Issue {
        id: id.into(),
        number: num.into(),
        status,
        blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let linked: HashSet<String> = ["t:A".to_string()].into();

    let old = vec![
        issue("t:A", "A", IssueStatus::Todo, &["t:B"]),
        issue("t:B", "B", IssueStatus::InProgress, &[]),
    ];
    let new = vec![
        issue("t:A", "A", IssueStatus::InProgress, &["t:B"]),
        issue("t:B", "B", IssueStatus::Done, &[]),
    ];
    let got = crate::hydrate_tracker::tracker_diff_notifications(&old, &new, &linked);
    // Linked issue A: its own status change + its blocker B resolving.
    let kinds: Vec<&str> = got.iter().map(|(k, _, _)| *k).collect();
    assert_eq!(kinds, vec!["status_changed", "blocker_resolved"]);
    assert!(got.iter().all(|(_, sr, _)| sr == "t:A"));

    // Re-running with old == new emits nothing (emit-once).
    assert!(crate::hydrate_tracker::tracker_diff_notifications(&new, &new, &linked).is_empty());
    // First fetch (empty old cache) emits nothing even with a Done blocker.
    assert!(crate::hydrate_tracker::tracker_diff_notifications(&[], &new, &linked).is_empty());
    // An unlinked issue's changes are silent.
    let unlinked: HashSet<String> = HashSet::new();
    assert!(crate::hydrate_tracker::tracker_diff_notifications(&old, &new, &unlinked).is_empty());
}
