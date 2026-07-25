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
        );
        assert_eq!(row, (true, 4, 1, Some("feat".into()), "/repo".into()));
        assert!(clean);
    }

    #[test]
    fn glyph_scan_no_upstream_is_zero_not_error() {
        // `Ok(None)` from ahead_behind is the genuine "no upstream" state: zero
        // arrows, and still a clean read.
        let prior: GlyphRow = (true, 4, 1, Some("feat".into()), "/repo".into());
        let (row, clean) = merge_glyph_scan(
            Some(&prior),
            Ok(false),
            Ok(None),
            Ok(Some("feat".into())),
            "/repo".into(),
        );
        assert_eq!(row, (false, 0, 0, Some("feat".into()), "/repo".into()));
        assert!(clean);
    }

    #[test]
    fn glyph_scan_transient_error_reuses_prior() {
        // A transient gix error on every read must reuse the prior row, not
        // collapse to zero/clean, and the row is NOT clean (cache untouched).
        let prior: GlyphRow = (true, 4, 1, Some("feat".into()), "/repo".into());
        let (row, clean) =
            merge_glyph_scan(Some(&prior), Err(()), Err(()), Err(()), "/repo".into());
        assert_eq!(row, (true, 4, 1, Some("feat".into()), "/repo".into()));
        assert!(!clean);
    }

    #[test]
    fn glyph_scan_partial_error_keeps_only_failed_field() {
        // ahead_behind errors (reuse prior counts) while dirty succeeds (fresh).
        let prior: GlyphRow = (true, 4, 1, Some("feat".into()), "/repo".into());
        let (row, clean) = merge_glyph_scan(
            Some(&prior),
            Ok(false),
            Err(()),
            Ok(Some("feat".into())),
            "/repo".into(),
        );
        assert_eq!(row, (false, 4, 1, Some("feat".into()), "/repo".into()));
        assert!(!clean);
    }

    #[test]
    fn glyph_scan_error_without_prior_falls_back_to_defaults() {
        // First-ever scan that errors has no prior to reuse: best-effort zeros,
        // and not clean so it won't be cached.
        let (row, clean) = merge_glyph_scan(None, Err(()), Err(()), Err(()), "/repo".into());
        assert_eq!(row, (false, 0, 0, None, "/repo".into()));
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
        let (session, seeded) = load_or_seed_session(&ws_dir);

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
    fn bootstrap_workspace_survives_switch_in_workspace_list() {
        // End-to-end regression for the disappearing-original-workspace bug:
        // bootstrap, switch to a second workspace, and the original must still
        // be listed (DB-backed, non-empty path) — not dropped as a stale live
        // fallback by merge_workspace_lists.
        let state_home =
            std::env::temp_dir().join(format!("tg-hydrate-survive-{}-state", std::process::id()));
        let ws_a =
            std::env::temp_dir().join(format!("tg-hydrate-survive-{}-a", std::process::id()));
        let ws_b =
            std::env::temp_dir().join(format!("tg-hydrate-survive-{}-b", std::process::id()));
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
        let (mut session, _) = load_or_seed_session(&ws_a);
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
