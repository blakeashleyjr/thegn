use super::*;
use crate::center::CenterTree;
use crate::hydrate::build_model;
use crate::session::{GroupKind, Session, WorktreeGroup};

#[test]
fn read_scoped_file_reads_inside_and_rejects_escape() {
    let dir = std::env::temp_dir().join(format!("sz-acp-read-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "hi there").unwrap();
    let wt = dir.to_str().unwrap();

    // Relative path resolves against the worktree root.
    assert_eq!(read_scoped_file(wt, "hello.txt").unwrap(), "hi there");
    // Absolute path inside the worktree is allowed.
    let abs = dir.join("hello.txt");
    assert_eq!(
        read_scoped_file(wt, abs.to_str().unwrap()).unwrap(),
        "hi there"
    );
    // A path that escapes the worktree (via ..) is rejected, not read.
    assert!(
        read_scoped_file(wt, "../../../etc/passwd").is_err(),
        "path escape must be rejected"
    );
    // A missing file is an error, not a panic.
    assert!(read_scoped_file(wt, "nope.txt").is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scoped_write_and_edit_stay_inside_worktree() {
    let dir = std::env::temp_dir().join(format!("sz-acp-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wt = dir.to_str().unwrap();

    // Write creates the file (and parent dirs) inside the worktree.
    write_scoped_file(wt, "sub/new.txt", "hello").unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(),
        "hello"
    );

    // Edit applies precise replacements; a missing match errors.
    let edits = serde_json::json!([{ "oldText": "hello", "newText": "goodbye" }]);
    apply_scoped_edits(wt, "sub/new.txt", &edits).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(),
        "goodbye"
    );
    let bad = serde_json::json!([{ "oldText": "absent", "newText": "x" }]);
    assert!(apply_scoped_edits(wt, "sub/new.txt", &bad).is_err());

    // Traversal escapes are rejected for both write and edit.
    assert!(write_scoped_file(wt, "../escape.txt", "x").is_err());
    assert!(apply_scoped_edits(wt, "../escape.txt", &edits).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_session_update_tracks_tool_and_usage() {
    use crate::chrome::{AgentActivity, AgentConn};
    use superzej_core::acp::methods::SessionUpdateEvent as E;
    let mut a = AgentActivity::default();

    // A tool call sets the tool name and marks it running.
    apply_agent_session_update(
        &mut a,
        E::ToolCall {
            tool_call_id: "1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({}),
        },
    );
    assert_eq!(a.last_tool.as_deref(), Some("bash"));
    assert!(a.running);
    // Folding a session update must not disturb the connection lifecycle.
    assert_eq!(a.conn, AgentConn::Online);

    // A completed update clears the running flag.
    apply_agent_session_update(
        &mut a,
        E::ToolCallUpdate {
            tool_call_id: "1".into(),
            status: "completed".into(),
            result: None,
        },
    );
    assert!(!a.running);

    // Usage updates feed the context-window percentage.
    apply_agent_session_update(&mut a, E::UsageUpdate { used: 5, size: 20 });
    assert_eq!((a.context_used, a.context_size), (5, 20));
    assert_eq!(a.last_tool.as_deref(), Some("bash")); // unchanged
    assert_eq!(a.conn, AgentConn::Online); // still untouched

    // Agent-end clears running (and is what drives the AgentDone notification).
    a.running = true;
    apply_agent_session_update(&mut a, E::AgentEnd { success: true });
    assert!(!a.running);
}

#[test]
fn issue_branch_tail_prefers_hint_then_slugifies() {
    // A provider branch hint is used verbatim (trimmed).
    assert_eq!(
        issue_branch_tail("ABC-1", "Fix the thing", Some("abc-1-fix-the-thing")),
        "abc-1-fix-the-thing"
    );
    // No hint → slug of number + title, dash-collapsed and lowercased.
    assert_eq!(
        issue_branch_tail("42", "Fix: the   Thing!", None),
        "42-fix-the-thing"
    );
    // Empty hint falls back to the slug.
    assert_eq!(issue_branch_tail("7", "Hi", Some("  ")), "7-hi");
    // Leading/trailing junk never yields edge dashes.
    let t = issue_branch_tail("#9", "!!!", None);
    assert!(!t.starts_with('-') && !t.ends_with('-'), "{t:?}");
}

// ---- test-run scope narrowing (item 518) -------------------------------
fn t_node(
    id: &str,
    label: &str,
    depth: usize,
    kind: crate::panel::TestNodeKind,
    state: crate::panel::TestState,
    path: Option<&str>,
) -> crate::panel::TestNode {
    crate::panel::TestNode {
        id: id.into(),
        label: label.into(),
        depth,
        kind,
        state,
        location: path.map(|p| crate::panel::TestLocation {
            path: p.into(),
            line: 1,
            column: None,
        }),
        message: None,
        placeholder: false,
    }
}

/// A group `mod` with one test child; `cursor` selects within `nodes` order.
fn tests_state(matcher_path: Option<&str>, cursor: usize) -> crate::panel::TestPanelState {
    use crate::panel::{TestNodeKind, TestState};
    let mut st = crate::panel::TestPanelState::default();
    st.nodes = vec![
        t_node(
            "mymod",
            "mymod",
            0,
            TestNodeKind::Group,
            TestState::Unknown,
            None,
        ),
        t_node(
            "mymod::test_a",
            "test_a",
            1,
            TestNodeKind::Test,
            TestState::Pass,
            matcher_path,
        ),
    ];
    st.cursor = cursor;
    st
}

#[test]
fn test_run_selected_appends_test_id() {
    let st = tests_state(Some("src/lib.rs"), 1);
    let base = crate::panel::TestTask::new("cargo test", "cargo test", "cargo-test");
    let task = test_task_for_run(&st, TestRun::Selected, base);
    assert!(
        task.command.ends_with("'mymod::test_a'"),
        "{}",
        task.command
    );
    assert!(task.name.contains("test_a"));
}

#[test]
fn test_run_package_targets_parent_group() {
    // Cursor on the test node → Package resolves to the enclosing `mymod`.
    let st = tests_state(Some("src/lib.rs"), 1);
    let base = crate::panel::TestTask::new("cargo test", "cargo test", "cargo-test");
    let task = test_task_for_run(&st, TestRun::Package, base);
    assert!(task.command.ends_with("'mymod'"), "{}", task.command);
    assert!(!task.command.contains("test_a"), "{}", task.command);
}

#[test]
fn test_run_file_uses_path_for_pytest() {
    let st = tests_state(Some("tests/test_x.py"), 1);
    let base = crate::panel::TestTask::new("pytest", "pytest", "pytest");
    let task = test_task_for_run(&st, TestRun::File, base);
    assert!(
        task.command.ends_with("'tests/test_x.py'"),
        "{}",
        task.command
    );
    assert!(task.name.contains("test_x.py"));
}

#[test]
fn test_run_file_falls_back_to_module_for_cargo() {
    // cargo has no file selector → File runs the enclosing module.
    let st = tests_state(Some("src/lib.rs"), 1);
    let base = crate::panel::TestTask::new("cargo test", "cargo test", "cargo-test");
    let task = test_task_for_run(&st, TestRun::File, base);
    assert!(task.command.ends_with("'mymod'"), "{}", task.command);
    assert!(!task.command.contains("lib.rs"), "{}", task.command);
}

#[test]
fn test_run_all_runs_whole_suite() {
    let st = tests_state(Some("src/lib.rs"), 1);
    let base = crate::panel::TestTask::new("cargo test", "cargo test", "cargo-test");
    let task = test_task_for_run(&st, TestRun::All, base.clone());
    assert_eq!(
        task.command, base.command,
        "All leaves the command untouched"
    );
}

#[test]
fn test_run_go_uses_run_flag() {
    let st = tests_state(Some("x_test.go"), 1);
    let base = crate::panel::TestTask::new("go test", "go test ./...", "go-test");
    let task = test_task_for_run(&st, TestRun::Selected, base);
    assert!(
        task.command.contains("-run 'mymod::test_a'"),
        "{}",
        task.command
    );
}

#[test]
fn commit_message_prefill_from_entities() {
    use superzej_core::semantic::{EntityChange, EntityKind, EntitySummary, Touch};
    // No entity summary → empty prefill (user types their own).
    let mut panel = crate::panel::PanelData::default();
    assert_eq!(commit_message_prefill(&panel), "");

    // With an entity summary → the structural message.
    panel.entities = Some(EntitySummary::new(vec![(
        "src/lib.rs".into(),
        vec![EntityChange {
            kind: EntityKind::Function,
            name: "go".into(),
            added: 4,
            deleted: 0,
            touch: Touch::Added,
            start_line: 1,
        }],
    )]));
    let msg = commit_message_prefill(&panel);
    assert!(msg.starts_with("add `go`"), "{msg}");
    assert!(msg.contains("src/lib.rs:"), "{msg}");
}

/// Tests that set `XDG_STATE_HOME` race each other (the env is process
/// global); serialize them. Crate-wide so `agent`'s sandbox tests (which
/// also redirect `XDG_STATE_HOME`) serialize against these too — a
/// per-module lock would leave that cross-module race open. Poisoning is
/// fine to ignore — the env is re-set by the next holder either way.
use crate::testenv::ENV_LOCK;

fn one_tab_session() -> Session {
    Session {
        id: "s1".into(),
        worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
        active: 0,
    }
}

fn two_worktree_session() -> Session {
    Session {
        id: "s1".into(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
            WorktreeGroup::new("app/feat", GroupKind::Branch, "/tmp/app-feat"),
        ],
        active: 0,
    }
}

#[test]
fn workspace_order_follows_pinned_sidebar_order() {
    // Two DB-backed workspaces; pinning the second floats it to the top of
    // the rendered tree. Shift+Alt+↑/↓ must step through *that* order — the
    // visible one — not the raw DB position order, so a pinned workspace
    // navigates first just as it renders first.
    let session = Session {
        id: "/tmp/app".into(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
            WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/lib"),
        ],
        active: 0,
    };
    let workspaces = vec![
        ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
        ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
    ];
    let view = crate::sidebar::ViewState {
        pins: vec!["lib".into()],
        ..Default::default()
    };
    let rows = crate::sidebar::build_rows(
        &session,
        &workspaces,
        &view,
        &crate::sidebar::SidebarStatus::default(),
        &[],
        &[],
        &[],
    );
    // Pinned "lib" workspace floats first; order is by repo path.
    assert_eq!(
        sidebar_workspace_order(&rows),
        vec!["/tmp/lib".to_string(), "/tmp/app".to_string()],
    );
}

#[test]
fn summon_workspace_target_picks_nth_visible_workspace() {
    // Same pinned layout: visible order is [lib, app] (Ctrl+1 → lib,
    // Ctrl+2 → app). The active workspace is "/tmp/app".
    let session = Session {
        id: "/tmp/app".into(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
            WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/lib"),
        ],
        active: 0,
    };
    let workspaces = vec![
        ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
        ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
    ];
    let view = crate::sidebar::ViewState {
        pins: vec!["lib".into()],
        ..Default::default()
    };
    let rows = crate::sidebar::build_rows(
        &session,
        &workspaces,
        &view,
        &crate::sidebar::SidebarStatus::default(),
        &[],
        &[],
        &[],
    );
    // The active workspace is identified by slug ("app"), not raw path.
    let active = Some("app");
    // Ctrl+1 → the first visible (pinned) workspace, "lib".
    assert_eq!(
        summon_workspace_target(&rows, 1, active),
        Some("/tmp/lib".to_string()),
    );
    // Ctrl+2 → "app", which IS the active one → no-op (None).
    assert_eq!(summon_workspace_target(&rows, 2, active), None);
    // Ctrl+2 from a different active workspace → switches to "app".
    assert_eq!(
        summon_workspace_target(&rows, 2, Some("other")),
        Some("/tmp/app".to_string()),
    );
    // n=0 and out-of-range are no-ops.
    assert_eq!(summon_workspace_target(&rows, 0, active), None);
    assert_eq!(summon_workspace_target(&rows, 3, active), None);
    assert_eq!(summon_workspace_target(&rows, 9, active), None);
}

/// A minimal terminal row for the unified-ring tests. `kind`/`connection`
/// drive the host grouping (see `sidebar::terminal_host`).
fn mk_term(name: &str, conn: &str, kind: &str) -> superzej_core::models::TerminalRow {
    superzej_core::models::TerminalRow {
        id: 0,
        name: name.into(),
        kind: kind.into(),
        connection_string: conn.into(),
        folder_id: None,
        created_at: 0,
        last_active: 0,
        position: 0,
        sandbox_backend: String::new(),
        env_name: String::new(),
    }
}

// Rebuild the ring + step helper the handler uses, purely.
fn ring_step(ring: &[RingStop], cur: usize, next_dir: bool) -> RingStop {
    let total = ring.len();
    let next = if next_dir {
        (cur + 1) % total
    } else {
        (cur + total - 1) % total
    };
    ring[next].clone()
}

#[test]
fn unified_ring_crosses_workspaces_and_terminals() {
    // One workspace + two terminal hosts (local sorts first, then "prod").
    // The ring is [app, local, prod]; stepping from the workspace crosses
    // into the terminals region and wraps back — the two sections read as
    // one. This is exactly the traversal that was silently a no-op before.
    let session = Session {
        id: "/tmp/app".into(),
        worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
        active: 0,
    };
    let workspaces = vec![("app".into(), "app".into(), "repo".into(), "/tmp/app".into())];
    let terminals = vec![
        mk_term("shell1", "", "local"),
        mk_term("box", "ssh dave@prod", "ssh"),
    ];
    let rows = crate::sidebar::build_rows(
        &session,
        &workspaces,
        &crate::sidebar::ViewState::default(),
        &crate::sidebar::SidebarStatus::default(),
        &[],
        &[],
        &terminals,
    );
    let ring = unified_ring(&rows, &terminals);
    assert_eq!(
        ring,
        vec![
            RingStop::Workspace {
                slug: "app".into(),
                repo_path: Some("/tmp/app".into()),
            },
            RingStop::TerminalHost {
                key: "local".into()
            },
            RingStop::TerminalHost { key: "prod".into() },
        ],
    );
    // From the workspace (resolved by slug), Next crosses into terminals…
    let cur = ring_current_index(&ring, Some("app"), None).unwrap();
    assert_eq!(cur, 0);
    assert_eq!(
        ring_step(&ring, cur, true),
        RingStop::TerminalHost {
            key: "local".into()
        },
    );
    // …and Prev wraps backward from the workspace onto the last terminal host.
    assert_eq!(
        ring_step(&ring, cur, false),
        RingStop::TerminalHost { key: "prod".into() },
    );
    // From a terminal host, resolution is by host key; Next wraps to the
    // workspace, crossing the boundary the other way.
    let cur_t = ring_current_index(&ring, None, Some("prod")).unwrap();
    assert_eq!(cur_t, 2);
    assert_eq!(
        ring_step(&ring, cur_t, true),
        RingStop::Workspace {
            slug: "app".into(),
            repo_path: Some("/tmp/app".into()),
        },
    );
}

#[test]
fn ring_resolves_by_slug_and_keeps_live_fallback() {
    // A live-fallback workspace (empty repo_path in the workspace list) is
    // DROPPED by `sidebar_workspace_order`, which is what made the old
    // `position(|p| *p == session.id)` lookup return None → silent no-op.
    // The ring KEEPS it (with `repo_path: None`) and resolves the current
    // position by slug, so the motion still works.
    let session = Session {
        id: "/tmp/app".into(),
        worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
        active: 0,
    };
    // Empty repo_path marks a live fallback (see hydrate::workspace_list).
    let workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
    let terminals = vec![mk_term("shell1", "", "local")];
    let rows = crate::sidebar::build_rows(
        &session,
        &workspaces,
        &crate::sidebar::ViewState::default(),
        &crate::sidebar::SidebarStatus::default(),
        &[],
        &[],
        &terminals,
    );
    // The old path-based order drops the live fallback entirely.
    assert!(sidebar_workspace_order(&rows).is_empty());
    // The ring keeps it, and slug resolution locates it (never a no-op).
    let ring = unified_ring(&rows, &terminals);
    assert_eq!(
        ring,
        vec![
            RingStop::Workspace {
                slug: "app".into(),
                repo_path: None,
            },
            RingStop::TerminalHost {
                key: "local".into()
            },
        ],
    );
    assert_eq!(ring_current_index(&ring, Some("app"), None), Some(0));
    // Next crosses into the terminals region rather than no-op.
    assert_eq!(
        ring_step(&ring, 0, true),
        RingStop::TerminalHost {
            key: "local".into()
        },
    );
}

#[test]
fn ring_current_index_none_when_active_absent() {
    // When neither the active slug nor host key is on screen, the resolver
    // returns None and the handler starts from 0 (still moves, never no-op).
    let ring = vec![
        RingStop::Workspace {
            slug: "app".into(),
            repo_path: Some("/tmp/app".into()),
        },
        RingStop::TerminalHost {
            key: "local".into(),
        },
    ];
    assert_eq!(ring_current_index(&ring, Some("ghost"), None), None);
    assert_eq!(ring_current_index(&ring, None, Some("ghost")), None);
}

#[test]
fn forgetting_closed_worktree_registry_prevents_restart_readoption() {
    let root = std::env::temp_dir().join(format!(
        "superzej-close-worktree-{}-{}",
        std::process::id(),
        now_secs()
    ));
    let repo = root.join("app");
    let feat = root.join("app-feat");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&feat).unwrap();
    let db = superzej_core::db::Db::open_at(&root.join("state/superzej.db")).unwrap();
    let repo_s = repo.to_string_lossy().into_owned();
    let feat_s = feat.to_string_lossy().into_owned();
    let mut session = Session {
        id: repo_s.clone(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, &repo_s),
            WorktreeGroup::new("app/feat", GroupKind::Branch, &feat_s),
        ],
        active: 1,
    };
    session.persist(&db, &repo_s, now_secs()).unwrap();
    db.put_worktree("app/feat", &repo_s, &feat_s, "feat", None, None)
        .unwrap();

    let closing = session.worktrees[1].clone();
    forget_worktree_group(&db, &session.id, &closing);
    session.close_active_group();
    session.persist(&db, &repo_s, now_secs()).unwrap();

    let resurrected = Session::resurrect(&db, &repo_s).unwrap();
    assert_eq!(
        resurrected
            .worktrees
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        vec!["app/home"]
    );
    assert!(
        db.worktree_for_tab(&superzej_core::db::session(), "app/feat")
            .unwrap()
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resurrect_orders_worktrees_by_persisted_position() {
    let root = std::env::temp_dir().join(format!(
        "superzej-resurrect-order-{}-{}",
        std::process::id(),
        now_secs()
    ));
    let repo = root.join("app");
    let alpha = root.join("app-alpha");
    let beta = root.join("app-beta");
    for d in [&repo, &alpha, &beta] {
        std::fs::create_dir_all(d).unwrap();
    }
    let db = superzej_core::db::Db::open_at(&root.join("state/superzej.db")).unwrap();
    let repo_s = repo.to_string_lossy().into_owned();
    let alpha_s = alpha.to_string_lossy().into_owned();
    let beta_s = beta.to_string_lossy().into_owned();

    let session = Session {
        id: repo_s.clone(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, &repo_s),
            WorktreeGroup::new("app/alpha", GroupKind::Branch, &alpha_s),
            WorktreeGroup::new("app/beta", GroupKind::Branch, &beta_s),
        ],
        active: 0,
    };
    session.persist(&db, &repo_s, now_secs()).unwrap();
    // Register both branches; positions are assigned in call order.
    db.put_worktree("app/alpha", &repo_s, &alpha_s, "alpha", None, None)
        .unwrap();
    db.put_worktree("app/beta", &repo_s, &beta_s, "beta", None, None)
        .unwrap();

    // home leads the raw vec; registered branches trail it in creation
    // order (alpha before beta). A newly-registered branch therefore always
    // appends at the bottom rather than jumping above older worktrees.
    let r = Session::resurrect(&db, &repo_s).unwrap();
    assert_eq!(
        r.worktrees
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        vec!["app/home", "app/alpha", "app/beta"]
    );

    // A manual reorder (swap positions) survives resurrect: beta now
    // precedes alpha.
    db.swap_worktree_positions(&alpha_s, &beta_s).unwrap();
    let r = Session::resurrect(&db, &repo_s).unwrap();
    assert_eq!(
        r.worktrees
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        vec!["app/home", "app/beta", "app/alpha"]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn center_context_hints_include_close_tab_and_split_controls() {
    let cfg = superzej_core::config::Config::default();
    let focus = crate::focus::FocusState::default();
    let panel = crate::panel::PanelUi::default();
    let hints = context_hints(&focus, &panel, &cfg);

    let has = |c: &str, l: &str| hints.iter().any(|(hc, hl)| hc == c && hl == l);
    assert!(has("Alt x", "close"), "hints were {hints:?}");
    assert!(has("Alt n", "split↓"), "hints were {hints:?}");
    assert!(has("Alt N", "split→"), "hints were {hints:?}");
}

#[test]
fn center_context_hints_follow_keybind_overrides() {
    let mut cfg = superzej_core::config::Config::default();
    cfg.keybinds
        .insert("close-worktree".into(), "Ctrl Alt x".into());
    let focus = crate::focus::FocusState::default();
    let panel = crate::panel::PanelUi::default();
    let hints = context_hints(&focus, &panel, &cfg);

    let has = |c: &str, l: &str| hints.iter().any(|(hc, hl)| hc == c && hl == l);
    assert!(has("Ctrl Alt x", "close"), "hints were {hints:?}");
    assert!(!has("Alt x", "close"), "hints were {hints:?}");
}

#[test]
fn pane_event_channel_capacity_is_small_for_backpressure() {
    assert_eq!(PANE_EVENT_CHANNEL_CAPACITY, 256);
}

#[test]
fn pane_event_channel_is_actually_bounded() {
    let (tx, mut rx) = tokio_mpsc::channel::<PaneEvent>(PANE_EVENT_CHANNEL_CAPACITY);
    for _ in 0..PANE_EVENT_CHANNEL_CAPACITY {
        tx.try_send(PaneEvent::Output(1, Vec::new()))
            .expect("within capacity should enqueue");
    }
    assert!(
        matches!(
            tx.try_send(PaneEvent::Output(1, Vec::new())),
            Err(tokio_mpsc::error::TrySendError::Full(_))
        ),
        "PTY readers must hit backpressure instead of buffering unbounded output"
    );
    assert!(rx.try_recv().is_ok(), "draining releases capacity");
    tx.try_send(PaneEvent::Output(1, Vec::new()))
        .expect("one drained slot should be reusable");
}

#[test]
fn drawer_pool_respects_zero_limit_and_evicts_oldest() {
    let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(PANE_EVENT_CHANNEL_CAPACITY);
    let mut panes = Panes::new(tx);
    let mut pool = DrawerPool::default();
    let a = std::path::Path::new("/tmp/a");
    let b = std::path::Path::new("/tmp/b");

    // limit 0 = no pooling; the just-hidden pane is torn down immediately.
    pool.stash(a, 1, 0, &mut panes);
    assert!(!pool.contains(a));

    // limit 1 keeps only the most recent; stashing b evicts a.
    pool.stash(a, 1, 1, &mut panes);
    assert!(pool.contains(a));
    pool.stash(b, 2, 1, &mut panes);
    assert!(!pool.contains(a));
    assert!(pool.contains(b));
    assert_eq!(pool.take(b), Some(2));
    assert!(!pool.contains(b));

    // remove_id forgets a pooled drawer whose yazi exited on its own.
    pool.stash(a, 3, 2, &mut panes);
    assert!(pool.remove_id(3));
    assert!(!pool.remove_id(3));
}

#[test]
fn font_palette_has_escape_ctrl_c_and_empty_q_cancels() {
    let mut p = crate::palette::Palette::new(vec![crate::palette::PaletteItem::new(
        "font:JetBrainsMono Nerd Font",
        "JetBrainsMono Nerd Font",
    )]);
    assert!(palette_cancel_key(&p, &KeyCode::Escape, Modifiers::NONE));
    assert!(palette_cancel_key(
        &p,
        &KeyCode::Char('\x1b'),
        Modifiers::NONE
    ));
    assert!(palette_cancel_key(&p, &KeyCode::Char('c'), Modifiers::CTRL));
    assert!(palette_cancel_key(&p, &KeyCode::Char('q'), Modifiers::NONE));

    p.push_char('j');
    assert!(!palette_cancel_key(
        &p,
        &KeyCode::Char('q'),
        Modifiers::NONE
    ));
}

#[test]
fn generic_command_palette_does_not_treat_plain_q_as_cancel() {
    let p = crate::palette::Palette::new(vec![crate::palette::PaletteItem::new("quit", "Quit")]);
    assert!(!palette_cancel_key(
        &p,
        &KeyCode::Char('q'),
        Modifiers::NONE
    ));
    assert!(palette_cancel_key(&p, &KeyCode::Escape, Modifiers::NONE));
    assert!(palette_cancel_key(
        &p,
        &KeyCode::Char('\x1b'),
        Modifiers::NONE
    ));
}

/// A SidebarState whose `persist` writes to a temp DB scope rather than the
/// user DB — set via XDG_STATE_HOME guarded by the test itself is avoided;
/// instead these tests exercise only in-memory state transitions and the
/// rebuilt row visibility (persistence is covered by db.rs::ui_state tests).
fn focused_state(model: &mut FrameModel, session: &Session) -> SidebarState {
    let mut sb = SidebarState {
        focused: true,
        ..Default::default()
    };
    sb.rebuild(model, session);
    sb
}

fn press(
    sb: &mut SidebarState,
    ch: char,
    model: &mut FrameModel,
    session: &Session,
) -> SidebarOutcome {
    sb.handle_key(&KeyCode::Char(ch), Modifiers::NONE, model, session)
}

#[test]
fn sidebar_filter_hides_nonmatching_rows() {
    let session = two_worktree_session();
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
    let mut sb = focused_state(&mut model, &session);

    press(&mut sb, '/', &mut model, &session);
    for c in "feat".chars() {
        press(&mut sb, c, &mut model, &session);
    }
    let visible: Vec<String> = model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible)
        .map(|r| r.label.clone())
        .collect();
    assert!(visible.contains(&"feat".to_string()));
    assert!(!visible.contains(&"home".to_string()));
}

#[test]
fn sidebar_enter_activates_cursor_row() {
    let session = two_worktree_session();
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
    let mut sb = focused_state(&mut model, &session);
    // Rows: app(ws), home, feat. Move to feat and Enter → activate it.
    press(&mut sb, 'j', &mut model, &session);
    press(&mut sb, 'j', &mut model, &session);
    let out = sb.handle_key(&KeyCode::Enter, Modifiers::NONE, &mut model, &session);
    match out {
        SidebarOutcome::Activate(crate::sidebar::RowTarget::Tab(gi, ti)) => {
            assert_eq!(session.worktrees[gi].name, "app/feat");
            assert_eq!(ti, 0);
        }
        _ => panic!("expected Activate"),
    }
    // Digit keys are no longer a hidden quick-jump (no numbers shown).
    assert!(matches!(
        press(&mut sb, '3', &mut model, &session),
        SidebarOutcome::NotHandled
    ));
}

fn three_worktree_session() -> Session {
    Session {
        id: "s1".into(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
            WorktreeGroup::new("app/alpha", GroupKind::Branch, "/tmp/app-alpha"),
            WorktreeGroup::new("app/beta", GroupKind::Branch, "/tmp/app-beta"),
        ],
        active: 2, // beta
    }
}

#[test]
fn move_active_worktree_reorders_within_workspace_and_anchors_home() {
    // Holds the env lock: move_active_worktree opens the user DB to persist
    // the swap; point it at a throwaway scope (the swap no-ops on unknown
    // paths — we assert the in-memory reorder, which is the user-visible bit).
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_home = std::env::temp_dir().join(format!(
        "superzej-move-wt-{}-{}",
        std::process::id(),
        now_secs()
    ));
    // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around the calls.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

    let mut session = three_worktree_session();
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
    let mut sb = SidebarState::default();
    sb.rebuild(&mut model, &session);

    // Move beta up: it swaps with alpha and remains active.
    assert!(sb.move_active_worktree(&mut model, &mut session, true));
    let order: Vec<&str> = session.worktrees.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(order, vec!["app/home", "app/beta", "app/alpha"]);
    assert_eq!(session.worktrees[session.active].name, "app/beta");
    // The cursor follows the moved worktree (now the active row) so the
    // highlight travels with the item rather than stranding on its old slot.
    assert_eq!(sb.cursor, visible_index_of_active(&model));
    assert!(sb.selected_row(&model).is_some_and(|r| r.active));

    // Move beta up again: the slot above is home — blocked, nothing moves.
    assert!(!sb.move_active_worktree(&mut model, &mut session, true));
    let order: Vec<&str> = session.worktrees.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(order, vec!["app/home", "app/beta", "app/alpha"]);

    unsafe { std::env::remove_var("XDG_STATE_HOME") };
    let _ = std::fs::remove_dir_all(&state_home);
}

#[test]
fn move_under_computed_sort_flips_to_manual() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_home = std::env::temp_dir().join(format!(
        "superzej-move-flip-{}-{}",
        std::process::id(),
        now_secs()
    ));
    // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around the calls.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

    let mut session = three_worktree_session();
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
    let mut sb = SidebarState::default();
    sb.view.sort = crate::sidebar::SortMode::Name;
    sb.rebuild(&mut model, &session);

    // Moving under a computed sort flips the workspace to Manual so the move
    // is visible and persists.
    assert!(sb.move_active_worktree(&mut model, &mut session, true));
    assert_eq!(sb.view.sort, crate::sidebar::SortMode::Manual);

    unsafe { std::env::remove_var("XDG_STATE_HOME") };
    let _ = std::fs::remove_dir_all(&state_home);
}

#[test]
fn sidebar_multiselect_marks_and_bulk_close_targets_marked() {
    let session = two_worktree_session();
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
    let mut sb = focused_state(&mut model, &session);
    // Move to the home worktree row (index 1) and mark it.
    press(&mut sb, 'j', &mut model, &session);
    press(&mut sb, ' ', &mut model, &session);
    assert!(model.sidebar_marked.contains(&1));
    // Move to feat (index 2) and mark it too.
    press(&mut sb, 'j', &mut model, &session);
    press(&mut sb, ' ', &mut model, &session);
    let out = sb.handle_key(&KeyCode::Char('X'), Modifiers::NONE, &mut model, &session);
    match out {
        SidebarOutcome::CloseGroups(t) => {
            assert_eq!(t.len(), 2);
        }
        _ => panic!("expected CloseGroups"),
    }
}

#[test]
fn sidebar_destructive_actions_reanchor_cursor_to_active_row() {
    let mut session = two_worktree_session();
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
    let mut sb = focused_state(&mut model, &session);
    sb.cursor = 0; // stale cursor on the workspace header after a delete/re-sort

    session.switch_to(1);
    refresh_tab_model(&mut model, &session, &mut sb);
    sb.focus_active_row(&mut model);

    let row = sb
        .selected_row(&model)
        .expect("active row should be visible");
    assert!(row.active, "cursor should land on active row, got {row:?}");
    assert_eq!(row.label, "feat");
}

#[test]
fn sidebar_width_adjust_clamps_and_relayouts() {
    // Persisting width opens the global DB; redirect it to a temp dir so the
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // test never touches the user's state (mirrors the other DB tests here).
    let state_home = std::env::temp_dir().join(format!("sz-host-width-{}", std::process::id()));
    // SAFETY: test is single-threaded; sets/clears an XDG var around calls.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

    let session = one_tab_session();
    let mut model = build_initial_model(&session, None);
    let mut sb = focused_state(&mut model, &session);
    // Narrow past the minimum: clamps at SIDEBAR_MIN_WIDTH.
    for _ in 0..20 {
        let _ = press(&mut sb, '<', &mut model, &session);
    }
    assert_eq!(sb.width, Some(crate::layout::SIDEBAR_MIN_WIDTH));
    let out = press(&mut sb, '>', &mut model, &session);
    assert!(matches!(out, SidebarOutcome::Relayout));

    // SAFETY: test is single-threaded.
    unsafe { std::env::remove_var("XDG_STATE_HOME") };
    let _ = std::fs::remove_dir_all(&state_home);
}

#[test]
fn effective_cols_per_sidebar_mode() {
    use crate::layout::{RAIL_COLS, SIDEBAR_COLS, SidebarMode};
    let mut sb = SidebarState::default();
    // Full (default): the layout default width.
    assert_eq!(sb.mode, SidebarMode::Full);
    assert_eq!(sb.effective_cols(160), SIDEBAR_COLS);
    // Rail: the fixed slim width regardless of window size or expand.
    sb.mode = SidebarMode::Rail;
    sb.expanded = true;
    assert_eq!(sb.effective_cols(160), RAIL_COLS);
    // Back to Full + expanded → ~half the window.
    sb.mode = SidebarMode::Full;
    assert_eq!(sb.effective_cols(160), 80);
}

#[test]
fn sidebar_e_toggles_wide_expand_and_persists() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Persisting the flag opens the global DB; redirect it to a temp dir.
    let state_home = std::env::temp_dir().join(format!("sz-host-expand-{}", std::process::id()));
    // SAFETY: test is single-threaded; sets/clears an XDG var around calls.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

    let session = one_tab_session();
    let mut model = build_initial_model(&session, None);
    let mut sb = focused_state(&mut model, &session);

    // Resting: not expanded; effective width is the layout default.
    assert!(!sb.expanded);
    assert_eq!(sb.effective_cols(160), crate::layout::SIDEBAR_COLS);

    // `e` flips to Wide and asks for a relayout; effective width ≈ half.
    let out = press(&mut sb, 'e', &mut model, &session);
    assert!(matches!(out, SidebarOutcome::Relayout));
    assert!(sb.expanded);
    assert_eq!(sb.effective_cols(160), 80);

    // `e` again restores.
    let _ = press(&mut sb, 'e', &mut model, &session);
    assert!(!sb.expanded);

    // The flag round-trips through the DB: re-expand, then a fresh state
    // loaded from the same scope comes back expanded.
    let _ = press(&mut sb, 'e', &mut model, &session);
    let db = superzej_core::db::Db::open().unwrap();
    let mut reloaded = SidebarState::default();
    reloaded.load(&db, SIDEBAR_SCOPE);
    assert!(reloaded.expanded, "expanded state restored from the DB");

    // A fine `<` nudge drops out of Wide so the change is visible.
    let _ = press(&mut sb, '<', &mut model, &session);
    assert!(!sb.expanded);

    // SAFETY: test is single-threaded.
    unsafe { std::env::remove_var("XDG_STATE_HOME") };
    let _ = std::fs::remove_dir_all(&state_home);
}

#[test]
fn sidebar_escape_defocuses() {
    let session = one_tab_session();
    let mut model = build_initial_model(&session, None);
    let mut sb = focused_state(&mut model, &session);
    let out = sb.handle_key(&KeyCode::Escape, Modifiers::NONE, &mut model, &session);
    assert!(matches!(out, SidebarOutcome::Defocus));
}

#[test]
fn workspace_pin_persists_globally_not_per_active_workspace() {
    // Regression: pins used to be keyed by `session.id` (the active
    // workspace's repo path), so a pin made while workspace A was active was
    // stranded under A's scope and never reloaded under any other workspace
    // — the sidebar rendered unpinned. Pins now live in the global
    // SIDEBAR_SCOPE: they reload regardless of the active workspace.
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_home = std::env::temp_dir().join(format!("sz-host-wspin-{}", std::process::id()));
    // SAFETY: test is single-threaded; sets/clears an XDG var around calls.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

    // Two DB-backed workspaces; the active one is "/tmp/app" (session.id).
    let session = Session {
        id: "/tmp/app".into(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
            WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/lib"),
        ],
        active: 0,
    };
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![
        ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
        ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
    ];
    let mut sb = focused_state(&mut model, &session);

    // Put the cursor on the "lib" workspace row and pin it (`p`).
    sb.cursor = model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible)
        .position(|r| r.kind == crate::sidebar::RowKind::Workspace && r.workspace_slug == "lib")
        .expect("lib workspace row is visible");
    press(&mut sb, 'p', &mut model, &session);

    // Immediate: "lib" floats above the active "app".
    assert!(sb.view.pins.contains(&"lib".to_string()));
    assert_eq!(
        sidebar_workspace_order(&model.sidebar_rows),
        vec!["/tmp/lib".to_string(), "/tmp/app".to_string()],
    );

    // The pin is written to the GLOBAL scope, NOT the active workspace's
    // (`/tmp/app`) scope — the heart of the fix.
    let db = superzej_core::db::Db::open().unwrap();
    let global = db.ui_state_in_scope(SIDEBAR_SCOPE).unwrap();
    assert!(
        global.iter().any(|(k, v)| k == "pin:lib" && v == "1"),
        "pin saved in the global sidebar scope",
    );
    let ws_scoped = db.ui_state_in_scope(&session.id).unwrap();
    assert!(
        !ws_scoped.iter().any(|(k, _)| k == "pin:lib"),
        "pin must NOT be saved under the active workspace's scope",
    );

    // It reloads from the global scope — independent of which workspace is
    // active at startup (the cross-workspace failure mode).
    let mut reloaded = SidebarState::default();
    reloaded.load(&db, SIDEBAR_SCOPE);
    assert!(
        reloaded.view.pins.contains(&"lib".to_string()),
        "pin restored from the global scope on reload",
    );

    // SAFETY: test is single-threaded.
    unsafe { std::env::remove_var("XDG_STATE_HOME") };
    let _ = std::fs::remove_dir_all(&state_home);
}

#[test]
fn load_or_seed_session_recovers_tabs_from_db_when_present() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_home = std::env::temp_dir().join(format!("test_db_{}", std::process::id()));
    let db_path = state_home.join("superzej/superzej.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let db = superzej_core::db::Db::open_at(&db_path).unwrap();
    let _ = db.put_workspace("/tmp/app", "app", "repo");
    // The worktree dir must exist on disk — vanished dirs are pruned at
    // load (git is the source of truth).
    let wt_dir = state_home.join("app-feat");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let wt_path = wt_dir.to_string_lossy().into_owned();
    db.put_tab_group(
        "/tmp/app",
        &superzej_core::models::TabGroupRow {
            name: "app/feat".into(),
            kind: "branch".into(),
            worktree: wt_path.clone(),
            ordinal: 0,
            active_tab: 0,
        },
    )
    .unwrap();
    db.put_group_tab(
        "/tmp/app",
        &superzej_core::models::GroupTabRow {
            group_name: "app/feat".into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        },
    )
    .unwrap();

    // SAFETY: test is single-threaded; sets/clears an XDG var around one call.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

    let (session, seeded) = load_or_seed_session(std::path::Path::new("/tmp/app"));

    unsafe { std::env::remove_var("XDG_STATE_HOME") };

    assert_eq!(session.worktrees.len(), 1);
    assert_eq!(session.worktrees[0].name, "app/feat");
    assert_eq!(session.id, "/tmp/app");
    assert!(!seeded, "a resurrected session is not a fresh seed");
}

#[test]
fn load_or_seed_session_ignores_launch_directory() {
    // Directory-agnostic: launching from an unrelated cwd reopens the
    // most-recently-active workspace, never a workspace keyed to the cwd.
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_home = std::env::temp_dir().join(format!("test_db_agnostic_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_home);
    let db_path = state_home.join("superzej/superzej.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let db = superzej_core::db::Db::open_at(&db_path).unwrap();
    // A registered workspace unrelated to the launch cwd below.
    let _ = db.put_workspace("/tmp/app", "app", "repo");
    let wt_dir = state_home.join("app-feat");
    std::fs::create_dir_all(&wt_dir).unwrap();
    db.put_tab_group(
        "/tmp/app",
        &superzej_core::models::TabGroupRow {
            name: "app/feat".into(),
            kind: "branch".into(),
            worktree: wt_dir.to_string_lossy().into_owned(),
            ordinal: 0,
            active_tab: 0,
        },
    )
    .unwrap();

    // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around one call.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
    // Launch from a directory unrelated to either workspace.
    let (session, _) = load_or_seed_session(std::path::Path::new("/tmp/somewhere-unrelated"));
    unsafe { std::env::remove_var("XDG_STATE_HOME") };
    let _ = std::fs::remove_dir_all(&state_home);

    assert_eq!(
        session.id, "/tmp/app",
        "should reopen the most-recently-active workspace, not the cwd"
    );
    assert!(
        session.worktrees.iter().any(|g| g.name == "app/feat"),
        "the recent workspace's worktree should be present"
    );
}

#[test]
fn load_or_seed_session_reports_fresh_seed() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_home =
        std::env::temp_dir().join(format!("test_db_seed_{}_state", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_home);
    std::fs::create_dir_all(state_home.join("superzej")).unwrap();

    // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around one call.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
    let (session, seeded) = load_or_seed_session(std::path::Path::new("/tmp/Fresh-Repo"));
    unsafe { std::env::remove_var("XDG_STATE_HOME") };

    assert!(seeded, "an empty DB seeds a fresh home group");
    assert_eq!(session.worktrees.len(), 1);
    assert_eq!(
        session.worktrees[0].name, "fresh-repo/home",
        "seeded home group is slug-keyed"
    );
}

#[test]
fn hydration_worker_loads_real_workspaces_into_sidebar() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_home =
        std::env::temp_dir().join(format!("test_db_sidebar_{}_state", std::process::id()));
    let db_path = state_home.join("superzej/superzej.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let db = superzej_core::db::Db::open_at(&db_path).unwrap();
    let _ = db.put_workspace("/tmp/repo1", "repo1", "repo");
    // Ensure some time passes so timestamps are distinctly different
    std::thread::sleep(std::time::Duration::from_millis(10));
    let _ = db.put_workspace("/tmp/repo2", "repo2", "repo");

    // SAFETY: test is single-threaded; sets/clears an XDG var around calls.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

    let (session, _) = load_or_seed_session(std::path::Path::new("/tmp/repo1"));
    let model = build_model(&session, &db, crate::hydrate::HydrateHints::default());

    unsafe { std::env::remove_var("XDG_STATE_HOME") };

    let slugs: Vec<&str> = model
        .sidebar_workspaces
        .iter()
        .map(|(s, _, _, _)| s.as_str())
        .collect();
    assert!(
        slugs.contains(&"repo1"),
        "Sidebar should contain repo1, got: {slugs:?}"
    );
    assert!(
        slugs.contains(&"repo2"),
        "Sidebar should contain repo2, got: {slugs:?}"
    );
}

#[test]
fn palette_worktree_switch_persists_active_tab_for_target_workspace() {
    let db_path = std::env::temp_dir().join(format!(
        "sj-host-palette-switch-{}-{}.sqlite",
        std::process::id(),
        now_secs()
    ));
    let db = superzej_core::db::Db::open_at(&db_path).unwrap();
    db.put_workspace("/tmp/repo-a", "repo-a", "repo").unwrap();
    db.put_workspace("/tmp/repo-b", "repo-b", "repo").unwrap();

    let row = |name: &str, ord: i64| superzej_core::models::TabGroupRow {
        name: name.into(),
        kind: "branch".into(),
        worktree: format!("/tmp/{name}"),
        ordinal: ord,
        active_tab: 0,
    };
    db.put_tab_group("/tmp/repo-b", &row("repo-b/home", 0))
        .unwrap();
    db.put_tab_group("/tmp/repo-b", &row("repo-b/feature-x", 1))
        .unwrap();

    let mut session = Session {
        id: "/tmp/repo-a".into(),
        worktrees: vec![WorktreeGroup::new(
            "repo-a/home",
            GroupKind::Home,
            "/tmp/repo-a",
        )],
        active: 0,
    };

    switch_to_workspace_tab(&mut session, &db, "/tmp/repo-b", "repo-b/feature-x").unwrap();

    assert_eq!(session.id, "/tmp/repo-b");
    assert_eq!(session.active_group().unwrap().name, "repo-b/feature-x");
    assert_eq!(
        db.active_tab("/tmp/repo-b").unwrap().as_deref(),
        Some("repo-b/feature-x")
    );
}

fn sidebar_labels(model: &FrameModel) -> Vec<String> {
    model.sidebar_rows.iter().map(|r| r.label.clone()).collect()
}

#[test]
fn refresh_tab_model_updates_sidebar_tree_when_tabs_change() {
    let mut session = one_tab_session();
    let mut model = build_initial_model(&session, None);
    let mut sb = SidebarState::default();

    refresh_tab_model(&mut model, &session, &mut sb);
    assert!(
        sidebar_labels(&model)
            .iter()
            .any(|row| row.contains("home")),
        "sidebar should show the initial home worktree: {:?}",
        sidebar_labels(&model)
    );

    session.add_group(WorktreeGroup::new(
        "app/feature-x",
        GroupKind::Branch,
        "/tmp/app-feature-x",
    ));
    refresh_tab_model(&mut model, &session, &mut sb);

    assert_eq!(model.worktree, "app/feature-x");
    assert!(
        sidebar_labels(&model)
            .iter()
            .any(|row| row.contains("feature-x")),
        "sidebar should include newly-created worktrees immediately: {:?}",
        sidebar_labels(&model)
    );
}

#[test]
fn removing_workspace_drops_its_rows_without_a_hydration() {
    // Regression: "Remove workspace" pruned the DB + live groups but the
    // sidebar row lingered, because refresh_tab_model rebuilds the workspace
    // list from a cached (non-DB) snapshot. The post-confirm handler now
    // prunes the cached lists synchronously; refresh_tab_model must then
    // render no trace of the removed workspace.
    let session = one_tab_session(); // only "app/home" is live
    let mut model = build_initial_model(&session, None);
    let mut sb = SidebarState::default();

    // Two DB-backed workspaces in the cached list; "lib" has no live group.
    model.sidebar_workspaces = vec![
        ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
        ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
    ];
    model.sidebar_db_worktrees = vec![crate::sidebar::DbWorktree {
        slug: "lib".into(),
        branch: "home".into(),
        repo_path: "/tmp/lib".into(),
        tab_name: "lib/home".into(),
        path: "/tmp/lib".into(),
        folder_id: None,
        sandbox_backend: None,
        env_name: None,
    }];

    // The synchronous prune the RemoveWorkspace handler performs (same code).
    forget_workspace_in_model(&mut model, "lib", "/tmp/lib");

    refresh_tab_model(&mut model, &session, &mut sb);

    assert!(
        !model.sidebar_rows.iter().any(|r| r.workspace_slug == "lib"),
        "removed workspace and its worktrees should vanish: {:?}",
        sidebar_labels(&model)
    );
    assert!(
        model
            .sidebar_rows
            .iter()
            .any(|r| r.kind == crate::sidebar::RowKind::Workspace && r.workspace_slug == "app"),
        "surviving workspace should still render: {:?}",
        sidebar_labels(&model)
    );
}

#[test]
fn workspace_worktree_dirs_skips_home_and_other_workspaces() {
    // The destructive "delete from disk" selection MUST exclude the home
    // checkout (path == repo_path — deleting it would nuke the main repo)
    // and any sibling workspace's worktrees. Only this workspace's branch
    // worktree dirs are returned.
    let db_path = std::env::temp_dir().join(format!(
        "sj-host-wtdirs-{}-{}.sqlite",
        std::process::id(),
        now_secs()
    ));
    let db = superzej_core::db::Db::open_at(&db_path).unwrap();
    // home checkout (path == repo_path) + two branch worktrees.
    db.put_worktree(
        "lib/home",
        "/tmp/repo-lib",
        "/tmp/repo-lib",
        "home",
        None,
        None,
    )
    .unwrap();
    db.put_worktree(
        "lib/feat",
        "/tmp/repo-lib",
        "/tmp/repo-lib-feat",
        "feat",
        None,
        None,
    )
    .unwrap();
    db.put_worktree(
        "lib/fix",
        "/tmp/repo-lib",
        "/tmp/repo-lib-fix",
        "fix",
        None,
        None,
    )
    .unwrap();
    // A sibling workspace's worktree must not be touched.
    db.put_worktree(
        "app/feat",
        "/tmp/repo-app",
        "/tmp/repo-app-feat",
        "feat",
        None,
        None,
    )
    .unwrap();

    let mut dirs = workspace_worktree_dirs(&db, "/tmp/repo-lib");
    dirs.sort();
    assert_eq!(
        dirs,
        vec![
            "/tmp/repo-lib-feat".to_string(),
            "/tmp/repo-lib-fix".to_string()
        ],
        "only this workspace's branch worktrees, never home or siblings"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn remove_workspace_with_db_prunes_db_and_closes_live_groups() {
    // The engine behind "Remove workspace": it must close every live group
    // the workspace owns AND prune its DB rows (workspaces row, worktree
    // registry, active-workspace pointer) so the workspace neither renders
    // nor resurrects — while leaving sibling workspaces untouched.
    let db_path = std::env::temp_dir().join(format!(
        "sj-host-remove-ws-{}-{}.sqlite",
        std::process::id(),
        now_secs()
    ));
    let db = superzej_core::db::Db::open_at(&db_path).unwrap();
    db.put_workspace("/tmp/repo-app", "app", "repo").unwrap();
    db.put_workspace("/tmp/repo-lib", "lib", "repo").unwrap();
    db.put_worktree(
        "lib/home",
        "/tmp/repo-lib",
        "/tmp/repo-lib",
        "home",
        None,
        None,
    )
    .unwrap();
    db.put_worktree(
        "lib/feat",
        "/tmp/repo-lib",
        "/tmp/repo-lib-feat",
        "feat",
        None,
        None,
    )
    .unwrap();
    db.put_worktree(
        "app/home",
        "/tmp/repo-app",
        "/tmp/repo-app",
        "home",
        None,
        None,
    )
    .unwrap();
    db.set_active_workspace("/tmp/repo-lib").unwrap();

    let mut session = Session {
        id: "/tmp/repo-lib".into(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/repo-app"),
            WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/repo-lib"),
            WorktreeGroup::new("lib/feat", GroupKind::Branch, "/tmp/repo-lib-feat"),
        ],
        active: 1,
    };
    let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(PANE_EVENT_CHANNEL_CAPACITY);
    let mut panes = Panes::new(tx);

    remove_workspace_with_db(&mut session, &mut panes, Some(&db), "/tmp/repo-lib", "lib");

    // Live groups: every "lib/*" group is closed; the sibling survives.
    let names: Vec<&str> = session.worktrees.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, vec!["app/home"], "all lib groups closed: {names:?}");

    // DB: the workspace row and its registry rows are gone; app stays.
    let ws: Vec<String> = db
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|w| w.repo_path)
        .collect();
    assert!(
        ws.contains(&"/tmp/repo-app".to_string()),
        "sibling kept: {ws:?}"
    );
    assert!(
        !ws.contains(&"/tmp/repo-lib".to_string()),
        "removed workspace row pruned: {ws:?}"
    );
    let wt_roots: Vec<String> = db
        .worktrees()
        .unwrap()
        .into_iter()
        .map(|w| w.repo_root)
        .collect();
    assert!(
        !wt_roots.iter().any(|p| p == "/tmp/repo-lib"),
        "registry rows pruned: {wt_roots:?}"
    );
    assert!(
        wt_roots.iter().any(|p| p == "/tmp/repo-app"),
        "sibling registry row kept: {wt_roots:?}"
    );

    // The active-workspace pointer (was the removed repo) is cleared.
    assert_eq!(db.active_workspace().unwrap(), None);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn remove_workspace_reports_orphan_count() {
    // The keep-files (non-destructive) removal MUST tell the user how many
    // worktrees survive on disk; the destructive removal never mentions them.
    assert_eq!(
        workspace_removed_status("lib", true, 0),
        "Removed workspace 'lib' (files kept on disk)"
    );
    assert_eq!(
        workspace_removed_status("lib", true, 1),
        "Removed workspace 'lib' (1 worktree remains on disk)"
    );
    assert_eq!(
        workspace_removed_status("lib", true, 3),
        "Removed workspace 'lib' (3 worktrees remain on disk)"
    );
    assert_eq!(
        workspace_removed_status("lib", false, 3),
        "Deleted workspace 'lib' (worktrees removed from disk)"
    );
}

#[test]
fn delete_last_workspace_empties_session() {
    // Removing the active (and only) workspace must fall back to an empty
    // home rather than leave the session pointing at a pruned workspace.
    let db_path = std::env::temp_dir().join(format!(
        "sj-host-last-ws-{}-{}.sqlite",
        std::process::id(),
        now_secs()
    ));
    let db = superzej_core::db::Db::open_at(&db_path).unwrap();
    // No workspace rows remain (the caller already pruned the last one).
    let mut session = Session {
        id: "/tmp/repo-lib".into(),
        worktrees: vec![WorktreeGroup::new(
            "lib/home",
            GroupKind::Home,
            "/tmp/repo-lib",
        )],
        active: 0,
    };

    land_after_workspace_removed(&mut session, Some(&db));

    assert!(session.id.is_empty(), "session id cleared");
    assert!(session.worktrees.is_empty(), "no groups remain");
    assert_eq!(session.active, 0);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn new_workspace_discovery_finds_repos_under_repo_roots() {
    // With repo_roots pointing at a dir containing a git repo, the fuzzy
    // picker's off-loop discovery source finds it.
    let tmp = std::env::temp_dir().join(format!(
        "sj-host-discover-{}-{}",
        std::process::id(),
        now_secs()
    ));
    let repo = tmp.join("myrepo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let repo_s = repo.to_string_lossy().into_owned();

    let cfg = superzej_core::config::Config {
        repo_roots: vec![tmp.to_string_lossy().into_owned()],
        repo_scan_depth: 3,
        ..Default::default()
    };
    let repos = superzej_core::repo::discover_repos(&cfg);
    assert!(
        repos.iter().any(|p| p == &repo_s),
        "discover_repos should find the temp repo: {repos:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// End-to-end input path for the Ctrl+1..9 workspace jump: raw terminal
/// bytes → termwiz parse → `normalize_key` → keymap dispatch → action. This
/// is the link the unit tests above can't cover — it proves a real
/// `modifyOtherKeys`/CSI-u terminal report for Ctrl+digit actually reaches
/// `Action::SummonWorkspace(n)` rather than collapsing to a control byte.
fn dispatch_bytes(bytes: &[u8]) -> crate::sequence::MatchResult {
    let mut parser = termwiz::input::InputParser::new();
    let evs = parser.parse_as_vec(bytes, false);
    let key = evs
        .into_iter()
        .find_map(|e| match e {
            InputEvent::Key(k) => Some(k),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no key event decoded from {bytes:?}"));
    let k = normalize_key(key);
    let input_key = crate::sequence::Key::modified(k.key, k.modifiers);
    crate::keymap::default_keymap().dispatch(crate::keymap::Mode::Normal, input_key)
}

#[test]
fn ctrl_digit_csi_u_bytes_reach_summon_workspace() {
    use crate::keymap::Action;
    use crate::sequence::MatchResult::Matched;
    // The CSI-u form a modifyOtherKeys/kitty terminal sends for Ctrl+<digit>
    // (`ESC [ <ascii> ; 5 u`, 5 = 1+ctrl-bit). 49/50/57 = '1'/'2'/'9'.
    assert_eq!(
        dispatch_bytes(b"\x1b[49;5u"),
        Matched(Action::SummonWorkspace(1)),
        "Ctrl+1 (CSI 49;5u) must reach SummonWorkspace(1)"
    );
    assert_eq!(
        dispatch_bytes(b"\x1b[50;5u"),
        Matched(Action::SummonWorkspace(2)),
        "Ctrl+2 (CSI 50;5u) must reach SummonWorkspace(2)"
    );
    assert_eq!(
        dispatch_bytes(b"\x1b[57;5u"),
        Matched(Action::SummonWorkspace(9)),
        "Ctrl+9 (CSI 57;5u) must reach SummonWorkspace(9)"
    );
    // The fixterms / modifyOtherKeys=2 alternate spelling (`CSI 27;5;<n>~`)
    // must decode the same way.
    assert_eq!(
        dispatch_bytes(b"\x1b[27;5;50~"),
        Matched(Action::SummonWorkspace(2)),
        "Ctrl+2 (CSI 27;5;50~) must reach SummonWorkspace(2)"
    );
}

#[test]
fn legacy_nul_byte_does_not_trigger_summon_workspace() {
    use crate::keymap::Action;
    use crate::sequence::MatchResult;
    // Without modifyOtherKeys a terminal sends Ctrl+2 as a bare NUL, which
    // is ambiguous with Ctrl+Space — `normalize_key` resolves it to
    // Ctrl+Space, NOT SummonWorkspace. This documents why szhost relies on
    // the CSI-u report (and why the digit is the discoverable affordance).
    let r = dispatch_bytes(b"\x00");
    assert_ne!(
        r,
        MatchResult::Matched(Action::SummonWorkspace(2)),
        "a bare NUL must not be read as Ctrl+2 → SummonWorkspace"
    );
}

#[test]
fn normalize_key_maps_nul_to_ctrl_space() {
    let nul = termwiz::input::KeyEvent {
        key: KeyCode::Char('\0'),
        modifiers: Modifiers::NONE,
    };
    let n = normalize_key(nul);
    assert_eq!(n.key, KeyCode::Char(' '));
    assert!(n.modifiers.contains(Modifiers::CTRL));
    // Already-decoded Ctrl+Space (kitty CSI-u) passes through unchanged.
    let kitty = termwiz::input::KeyEvent {
        key: KeyCode::Char(' '),
        modifiers: Modifiers::CTRL,
    };
    let k = normalize_key(kitty.clone());
    assert_eq!(k.key, kitty.key);
    assert_eq!(k.modifiers, kitty.modifiers);
}

#[test]
fn normalize_key_canonicalizes_kitty_csi_u_control_chars() {
    // Shift+Tab over the kitty protocol arrives as Char('\t') + SHIFT; it
    // must become Tab + SHIFT so key_bytes emits the `ESC [ Z` back-tab
    // (and so apps like Claude Code see a real Shift+Tab, not a plain Tab).
    let shift_tab = normalize_key(termwiz::input::KeyEvent {
        key: KeyCode::Char('\t'),
        modifiers: Modifiers::SHIFT,
    });
    assert_eq!(shift_tab.key, KeyCode::Tab);
    assert_eq!(shift_tab.modifiers, Modifiers::SHIFT);
    assert_eq!(
        crate::input::key_bytes(&shift_tab.key, shift_tab.modifiers).unwrap(),
        b"\x1b[Z"
    );
    // The same canonicalization covers the other ASCII control keys.
    assert_eq!(
        normalize_key(termwiz::input::KeyEvent {
            key: KeyCode::Char('\r'),
            modifiers: Modifiers::SHIFT,
        })
        .key,
        KeyCode::Enter
    );
    assert_eq!(
        normalize_key(termwiz::input::KeyEvent {
            key: KeyCode::Char('\x7f'),
            modifiers: Modifiers::CTRL,
        })
        .key,
        KeyCode::Backspace
    );
    // An unmodified control char (a literal Tab/Esc) is left untouched so
    // is_escape_key and the plain-Tab path keep working.
    assert_eq!(
        normalize_key(termwiz::input::KeyEvent {
            key: KeyCode::Char('\t'),
            modifiers: Modifiers::NONE,
        })
        .key,
        KeyCode::Char('\t')
    );
}

// ── drain helpers ────────────────────────────────────────────────────────

fn mk_key(code: KeyCode) -> InputEvent {
    InputEvent::Key(termwiz::input::KeyEvent {
        key: code,
        modifiers: Modifiers::NONE,
    })
}

fn mk_wheel(up: bool) -> InputEvent {
    use termwiz::input::{MouseButtons, MouseEvent};
    let mut buttons = MouseButtons::VERT_WHEEL;
    if up {
        buttons |= MouseButtons::WHEEL_POSITIVE;
    }
    InputEvent::Mouse(MouseEvent {
        x: 1,
        y: 1,
        mouse_buttons: buttons,
        modifiers: Modifiers::NONE,
    })
}

#[test]
fn drain_key_repeats_coalesces_identical_keys() {
    let key = termwiz::input::KeyEvent {
        key: KeyCode::DownArrow,
        modifiers: Modifiers::NONE,
    };
    // Three identical repeats then a different key.
    let mut q: std::collections::VecDeque<InputEvent> = [
        mk_key(KeyCode::DownArrow),
        mk_key(KeyCode::DownArrow),
        mk_key(KeyCode::Char('x')),
    ]
    .into();
    let (n, leftover) = drain_key_repeats(&key, || q.pop_front());
    assert_eq!(n, 3);
    assert!(matches!(
        leftover,
        Some(InputEvent::Key(k)) if k.key == KeyCode::Char('x')
    ));
    // Empty queue → just the first (count = 1).
    let (n, leftover) = drain_key_repeats(&key, || None);
    assert_eq!(n, 1);
    assert!(leftover.is_none());
}

#[test]
fn drain_key_repeats_stops_on_different_modifiers() {
    let key = termwiz::input::KeyEvent {
        key: KeyCode::DownArrow,
        modifiers: Modifiers::NONE,
    };
    // Same key but with Shift — must stop the drain.
    let shifted = InputEvent::Key(termwiz::input::KeyEvent {
        key: KeyCode::DownArrow,
        modifiers: Modifiers::SHIFT,
    });
    let mut q: std::collections::VecDeque<InputEvent> =
        [mk_key(KeyCode::DownArrow), shifted].into();
    let (n, leftover) = drain_key_repeats(&key, || q.pop_front());
    assert_eq!(n, 2); // first + one plain repeat
    assert!(matches!(
        leftover,
        Some(InputEvent::Key(k)) if k.modifiers == Modifiers::SHIFT
    ));
}

#[test]
fn drain_wheel_ticks_coalesces_same_direction() {
    // 4 up ticks then a down tick.
    let mut q: std::collections::VecDeque<InputEvent> = [
        mk_wheel(true),
        mk_wheel(true),
        mk_wheel(true),
        mk_wheel(false), // opposite direction — stops drain
    ]
    .into();
    let (n, leftover) = drain_wheel_ticks(true, || q.pop_front());
    assert_eq!(n, 4, "first tick + 3 repeats = 4 total");
    assert!(
        matches!(&leftover, Some(InputEvent::Mouse(m)) if !m.mouse_buttons.contains(termwiz::input::MouseButtons::WHEEL_POSITIVE)),
        "leftover should be the down-wheel event"
    );
    // The leftover is back in the caller's hands; the queue should be empty now.
    assert!(q.is_empty());
}

#[test]
fn drain_wheel_ticks_stops_on_direction_reversal() {
    // Only one tick in the queue before a reversal.
    let mut q: std::collections::VecDeque<InputEvent> = [mk_wheel(false)].into();
    let (n, leftover) = drain_wheel_ticks(true, || q.pop_front());
    // count = 1 (the original event the caller already consumed), leftover = the down.
    assert_eq!(n, 1);
    assert!(leftover.is_some());
}

#[test]
fn drain_wheel_ticks_stops_on_non_wheel_event() {
    // A keypress interrupts the wheel drain.
    let mut q: std::collections::VecDeque<InputEvent> =
        [mk_wheel(true), mk_key(KeyCode::Char('q'))].into();
    let (n, leftover) = drain_wheel_ticks(true, || q.pop_front());
    assert_eq!(n, 2, "first + one more wheel = 2");
    assert!(matches!(leftover, Some(InputEvent::Key(_))));
}

#[test]
fn drain_wheel_ticks_empty_queue_returns_one() {
    let (n, leftover) = drain_wheel_ticks(true, || None);
    assert_eq!(n, 1);
    assert!(leftover.is_none());
}

#[test]
fn drain_wheel_ticks_down_direction() {
    // Symmetric: draining down-wheel events.
    let mut q: std::collections::VecDeque<InputEvent> =
        [mk_wheel(false), mk_wheel(false), mk_wheel(true)].into();
    let (n, leftover) = drain_wheel_ticks(false, || q.pop_front());
    assert_eq!(n, 3);
    assert!(
        matches!(&leftover, Some(InputEvent::Mouse(m)) if m.mouse_buttons.contains(termwiz::input::MouseButtons::WHEEL_POSITIVE))
    );
}

fn mk_drag(x: u16, y: u16) -> InputEvent {
    use termwiz::input::{MouseButtons, MouseEvent};
    InputEvent::Mouse(MouseEvent {
        x,
        y,
        mouse_buttons: MouseButtons::LEFT,
        modifiers: Modifiers::NONE,
    })
}

fn mk_release(x: u16, y: u16) -> InputEvent {
    use termwiz::input::{MouseButtons, MouseEvent};
    InputEvent::Mouse(MouseEvent {
        x,
        y,
        mouse_buttons: MouseButtons::NONE,
        modifiers: Modifiers::NONE,
    })
}

fn first_drag(x: u16, y: u16) -> termwiz::input::MouseEvent {
    match mk_drag(x, y) {
        InputEvent::Mouse(m) => m,
        _ => unreachable!(),
    }
}

#[test]
fn drain_drag_keeps_only_the_latest_position() {
    // Three drag samples queued behind the first; only the last position
    // should survive, and the release is handed back to requeue.
    let mut q: std::collections::VecDeque<InputEvent> =
        [mk_drag(5, 5), mk_drag(7, 9), mk_release(7, 9)].into();
    let (last, leftover) = drain_drag_events(first_drag(2, 2), || q.pop_front());
    assert_eq!((last.x, last.y), (7, 9), "latest drag wins");
    assert!(
        matches!(&leftover, Some(InputEvent::Mouse(m)) if !m.mouse_buttons.contains(termwiz::input::MouseButtons::LEFT)),
        "the release stops the drain and is returned"
    );
    assert!(q.is_empty());
}

#[test]
fn drain_drag_empty_queue_returns_first() {
    let (last, leftover) = drain_drag_events(first_drag(3, 4), || None);
    assert_eq!((last.x, last.y), (3, 4));
    assert!(leftover.is_none());
}

#[test]
fn drain_drag_stops_on_non_drag_event() {
    // A keypress mid-drag interrupts coalescing and is returned.
    let mut q: std::collections::VecDeque<InputEvent> =
        [mk_drag(6, 6), mk_key(KeyCode::Char('x'))].into();
    let (last, leftover) = drain_drag_events(first_drag(1, 1), || q.pop_front());
    assert_eq!((last.x, last.y), (6, 6));
    assert!(matches!(leftover, Some(InputEvent::Key(_))));
}

#[test]
fn drain_event_repeats_stops_on_first_mismatch() {
    // Exercise the generic core directly with an arbitrary predicate over
    // InputEvent: accept only Char('a') keys; stop on Char('b').
    let mut q: std::collections::VecDeque<InputEvent> = [
        mk_key(KeyCode::Char('a')),
        mk_key(KeyCode::Char('a')),
        mk_key(KeyCode::Char('b')), // mismatch → leftover
        mk_key(KeyCode::Char('a')), // unreachable in this drain
    ]
    .into();
    let (n, leftover) = drain_event_repeats(
        |ev| matches!(ev, InputEvent::Key(k) if k.key == KeyCode::Char('a')),
        || q.pop_front(),
    );
    assert_eq!(n, 3); // first + 2 repeats
    assert!(matches!(
        leftover,
        Some(InputEvent::Key(k)) if k.key == KeyCode::Char('b')
    ));
    // The 4th event is still in the queue (not consumed by the drain).
    assert_eq!(q.len(), 1);
}

#[test]
fn drain_event_repeats_all_matching_drains_to_empty() {
    let mut q: std::collections::VecDeque<InputEvent> =
        [mk_key(KeyCode::Char('a')), mk_key(KeyCode::Char('a'))].into();
    let (n, leftover) = drain_event_repeats(
        |ev| matches!(ev, InputEvent::Key(k) if k.key == KeyCode::Char('a')),
        || q.pop_front(),
    );
    assert_eq!(n, 3); // first + 2 from queue
    assert!(leftover.is_none());
    assert!(q.is_empty());
}

#[test]
fn prune_vanished_group_lands_on_home_and_returns_pane_ids() {
    let mut session = Session {
        id: "/r/app".into(),
        worktrees: vec![
            WorktreeGroup::new("app/home", GroupKind::Home, "/r/app"),
            WorktreeGroup::new("app/feat", GroupKind::Branch, "/wt/feat"),
        ],
        active: 1,
    };
    // Point the doomed group's tab at a known pane id.
    session.worktrees[1].tabs[0].center = crate::center::CenterTree::Leaf(7);

    let ids = prune_vanished_group(&mut session, 1);
    assert_eq!(ids, vec![7]);
    assert_eq!(session.worktrees.len(), 1);
    assert_eq!(session.active_group().unwrap().name, "app/home");

    // Out of range is a no-op.
    assert!(prune_vanished_group(&mut session, 9).is_empty());
    assert_eq!(session.worktrees.len(), 1);
}

#[test]
fn workspace_pool_stash_take_roundtrips_trees() {
    let mut pool = WorkspacePool::default();
    assert!(!pool.contains("/r/a"));
    pool.stash(
        "/r/a".into(),
        ResidentWorkspace {
            worktrees: vec![WorktreeGroup::new("a/home", GroupKind::Home, "/r/a")],
            active: 0,
        },
    );
    assert!(pool.contains("/r/a"));
    let rw = pool.take("/r/a").expect("stashed");
    assert_eq!(rw.worktrees.len(), 1);
    assert_eq!(rw.worktrees[0].name, "a/home");
    assert!(!pool.contains("/r/a"), "take removes the entry");
    assert!(pool.take("/r/a").is_none());
}

#[test]
fn remap_cold_workspace_ids_moves_ids_past_the_live_range() {
    // A cold-resurrected workspace's persisted ids must be rewritten onto a
    // fresh range so they can't alias the live panes of a parked workspace.
    let mut session = two_worktree_session();
    session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(3);
    session.worktrees[0].tabs[0].focused_pane = 3;
    session.worktrees[0].tabs[0]
        .pane_cwds
        .insert(3, "/x".into());
    session.worktrees[1].tabs[0].center = crate::center::CenterTree::Leaf(8);

    let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(16);
    let mut panes = Panes::new(tx);
    // Pretend ids 1..=10 are already issued to live panes.
    let reserved = panes.reserve_ids(10);
    assert_eq!(reserved, 1);

    remap_cold_workspace_ids(&mut session, &mut panes);

    let mut ids: Vec<u32> = session
        .worktrees
        .iter()
        .flat_map(|g| g.tabs.iter())
        .flat_map(|t| t.center.pane_ids())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids.len(), 2);
    assert!(
        ids.iter().all(|&id| id > 10),
        "remapped ids are disjoint from the live range: {ids:?}"
    );
    // The cwd key and focus followed the leaf through the remap.
    let new0 = session.worktrees[0].tabs[0].center.pane_ids()[0];
    assert_eq!(session.worktrees[0].tabs[0].focused_pane, new0);
    assert_eq!(
        session.worktrees[0].tabs[0]
            .pane_cwds
            .get(&new0)
            .map(String::as_str),
        Some("/x")
    );
}

#[test]
fn initial_resurrect_remap_prevents_fresh_spawns_aliasing_restored_ids() {
    // Repro of the cross-worktree mirror after a restart: a resurrected
    // session carries pane ids the PREVIOUS process allocated from 1, while
    // this process's `Panes` also starts `next_id` at 1. Two different
    // worktrees here hold low restored ids; without the up-front remap a
    // freshly-spawned pane would reuse one of them, and `missing_leaves`
    // would skip the still-unmaterialized tab — aliasing two trees onto one
    // live PtyPane (the mirror).
    let mut session = two_worktree_session();
    session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(1);
    session.worktrees[0].tabs[0].focused_pane = 1;
    session.worktrees[1].tabs[0].center = crate::center::CenterTree::Leaf(2);
    session.worktrees[1].tabs[0].focused_pane = 2;

    let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(16);
    let mut panes = Panes::new(tx); // fresh registry, next_id == 1

    remap_cold_workspace_ids(&mut session, &mut panes);

    // Every restored leaf id is unique across the whole session.
    let mut ids: Vec<u32> = session
        .worktrees
        .iter()
        .flat_map(|g| g.tabs.iter())
        .flat_map(|t| t.center.pane_ids())
        .collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "restored ids are unique: {ids:?}");

    // The next id any future spawn (pin/drawer/shell) will take is past
    // every restored leaf — so a fresh pane can never alias a tab tree.
    let next = panes.reserve_ids(1);
    assert!(
        ids.iter().all(|&id| id < next),
        "fresh spawn id {next} is disjoint from restored ids {ids:?}"
    );
}

#[test]
fn workspace_switch_does_not_duplicate_sidebar_workspaces() {
    // Post-switch state: the DB-hydrated list already names both
    // workspaces; the session now holds only the switched-to workspace's
    // (slug-keyed) live home group.
    let session = Session {
        id: "/r/washu".into(),
        worktrees: vec![WorktreeGroup::new(
            "washu/home",
            GroupKind::Home,
            "/r/washu",
        )],
        active: 0,
    };
    let mut model = build_initial_model(&session, None);
    model.sidebar_workspaces = vec![
        (
            "superzej".into(),
            "superzej".into(),
            "repo".into(),
            "/r/superzej".into(),
        ),
        (
            "washu".into(),
            "WASHU".into(),
            "repo".into(),
            "/r/washu".into(),
        ),
    ];
    let mut sb = SidebarState::default();

    // Refresh repeatedly (every hydration intake calls this): the list
    // must stay stable — the old behavior appended a duplicate per call.
    refresh_tab_model(&mut model, &session, &mut sb);
    refresh_tab_model(&mut model, &session, &mut sb);

    let slugs: Vec<_> = model
        .sidebar_workspaces
        .iter()
        .map(|(s, _, _, _)| s.as_str())
        .collect();
    assert_eq!(slugs, vec!["superzej", "washu"]);

    let home_rows: Vec<_> = model
        .sidebar_rows
        .iter()
        .filter(|r| r.label == "home" && r.workspace_slug == "washu")
        .collect();
    assert_eq!(
        home_rows.len(),
        1,
        "exactly one home row for the live workspace: {:?}",
        sidebar_labels(&model)
    );
    assert!(home_rows[0].active, "and it is the active (live) row");
}

#[test]
fn new_tab_stays_within_the_worktree_and_tabbar_scopes_to_it() {
    let mut session = two_worktree_session();
    let mut model = build_initial_model(&session, None);
    let mut sb = SidebarState::default();

    // A new tab in the active worktree (Alt+t): the tabbar shows ONLY this
    // worktree's chips, never other worktrees.
    session.active_group_mut().unwrap().add_tab();
    refresh_tab_model(&mut model, &session, &mut sb);

    assert_eq!(model.worktree, "app/home");
    assert_eq!(model.tabs, vec!["1".to_string(), "2".to_string()]);
    assert_eq!(model.active_tab, 1);

    // Switching worktree swaps the whole strip (tabs live WITHIN a worktree).
    session.next_worktree();
    refresh_tab_model(&mut model, &session, &mut sb);
    assert_eq!(model.worktree, "app/feat");
    assert_eq!(model.tabs, vec!["1".to_string()]);
    assert_eq!(model.active_tab, 0);

    // And switching back restores the remembered tab.
    session.prev_worktree();
    refresh_tab_model(&mut model, &session, &mut sb);
    assert_eq!(model.worktree, "app/home");
    assert_eq!(model.active_tab, 1);
}

#[test]
fn tab_switch_refreshes_model_without_changing_chrome_layout() {
    let mut session = one_tab_session();
    session.add_group(WorktreeGroup::new(
        "app/feat",
        GroupKind::Branch,
        "/tmp/app-feat",
    ));
    let mut model = build_initial_model(&session, None);
    let mut sb = SidebarState::default();
    let chrome = layout::compute(160, 40, true, true);
    let before = chrome.clone();

    session.switch_to(1);
    refresh_tab_model(&mut model, &session, &mut sb);

    assert_eq!(model.worktree, "app/feat");
    assert_eq!(model.tabs, vec!["1".to_string()]);
    assert_eq!(
        chrome, before,
        "worktree switches must reuse the chrome snapshot"
    );
    assert_eq!(chrome.panel.unwrap().cols, layout::PANEL_COLS);
}

#[test]
fn dirty_ui_frames_render_before_pty_drain() {
    assert!(render_before_pty_drain(true));
    assert!(!render_before_pty_drain(false));
}

#[test]
fn warmed_tab_remap_rewrites_tree_and_focus() {
    let mut tab = crate::session::Tab::new("1");
    tab.center = CenterTree::Split {
        dir: crate::center::Dir::Row,
        children: vec![
            crate::center::Branch {
                weight: 1.0,
                child: CenterTree::Leaf(3),
            },
            crate::center::Branch {
                weight: 1.0,
                child: CenterTree::Leaf(4),
            },
        ],
    };
    tab.focused_pane = 4;

    assert!(remap_warmed_tab_ids(&mut tab, 4, &[(3, 20), (4, 21)]));

    assert_eq!(tab.center.pane_ids(), vec![20, 21]);
    assert_eq!(tab.focused_pane, 21);
}

#[test]
fn warmed_tab_remap_rejects_stale_tree() {
    let mut tab = crate::session::Tab::new("1");
    tab.center = CenterTree::Leaf(99);
    tab.focused_pane = 99;
    let before = tab.clone();

    assert!(!remap_warmed_tab_ids(&mut tab, 4, &[(3, 20), (4, 21)]));
    assert_eq!(tab, before);
}

#[test]
fn crash_count_fast_exits_increment_and_reach_limit() {
    let mut counts = std::collections::HashMap::new();
    let key = (0usize, 0usize);
    let threshold = std::time::Duration::from_secs(2);

    // Three consecutive fast crashes (< 2s) hit the limit.
    assert_eq!(
        update_crash_count(
            &mut counts,
            key,
            std::time::Duration::from_millis(50),
            threshold
        ),
        1
    );
    assert_eq!(
        update_crash_count(
            &mut counts,
            key,
            std::time::Duration::from_millis(50),
            threshold
        ),
        2
    );
    assert_eq!(
        update_crash_count(
            &mut counts,
            key,
            std::time::Duration::from_millis(50),
            threshold
        ),
        3
    );
    assert_eq!(counts[&key], 3);
}

#[test]
fn crash_count_slow_exit_resets_after_fast_crashes() {
    let mut counts = std::collections::HashMap::new();
    let key = (0usize, 0usize);
    let threshold = std::time::Duration::from_secs(2);

    // Two fast crashes build up the count.
    update_crash_count(
        &mut counts,
        key,
        std::time::Duration::from_millis(50),
        threshold,
    );
    update_crash_count(
        &mut counts,
        key,
        std::time::Duration::from_millis(50),
        threshold,
    );
    assert_eq!(counts[&key], 2);

    // A slow exit (user typed `exit` normally) resets the counter.
    let remaining = update_crash_count(
        &mut counts,
        key,
        std::time::Duration::from_secs(10),
        threshold,
    );
    assert_eq!(remaining, 0);
    assert!(!counts.contains_key(&key));
}

#[test]
fn crash_count_zero_age_treated_as_fast_crash() {
    // pane_age() returns None → unwrap_or_default() → Duration::ZERO.
    // A pane with no recorded spawn time exiting should count as a crash.
    let mut counts = std::collections::HashMap::new();
    let key = (1usize, 2usize);
    let threshold = std::time::Duration::from_secs(2);
    let result = update_crash_count(&mut counts, key, std::time::Duration::ZERO, threshold);
    assert_eq!(result, 1);
}

#[test]
fn crash_count_keys_are_independent() {
    let mut counts = std::collections::HashMap::new();
    let threshold = std::time::Duration::from_secs(2);
    let fast = std::time::Duration::from_millis(50);
    let key_a = (0usize, 0usize);
    let key_b = (0usize, 1usize);

    update_crash_count(&mut counts, key_a, fast, threshold);
    update_crash_count(&mut counts, key_a, fast, threshold);
    update_crash_count(&mut counts, key_b, fast, threshold);

    assert_eq!(counts[&key_a], 2);
    assert_eq!(counts[&key_b], 1);

    // A slow exit on key_b doesn't affect key_a.
    update_crash_count(
        &mut counts,
        key_b,
        std::time::Duration::from_secs(10),
        threshold,
    );
    assert_eq!(counts[&key_a], 2);
    assert!(!counts.contains_key(&key_b));
}
