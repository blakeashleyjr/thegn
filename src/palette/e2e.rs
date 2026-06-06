//! In-process end-to-end tests: drive the real `Palette` component through
//! iocraft's mock terminal with scripted key events and assert on the rendered
//! frames + the resulting chosen action. This exercises the full render/event
//! loop, `Core`, the engine, every source, preview, and frecency — headlessly,
//! with no real terminal or zellij session.

use super::app;
use super::item::Action;
use super::testutil;
use super::{Core, Shared};
use futures::StreamExt;
use iocraft::prelude::*;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn key(c: char) -> TerminalEvent {
    TerminalEvent::Key(KeyEvent::new(KeyEventKind::Press, KeyCode::Char(c)))
}
fn special(code: KeyCode) -> TerminalEvent {
    TerminalEvent::Key(KeyEvent::new(KeyEventKind::Press, code))
}
fn typed(s: &str) -> Vec<TerminalEvent> {
    s.chars().map(key).collect()
}
fn resize() -> TerminalEvent {
    TerminalEvent::Resize(120, 40)
}
/// Filler render ticks (also gives streaming sources wall-clock to push).
fn ticks(n: usize) -> Vec<TerminalEvent> {
    (0..n).map(|_| resize()).collect()
}

/// Run `body` (a leading resize is prepended; caller must end with Esc/Enter so
/// the loop exits) and return every rendered frame plus the shared state.
fn run(body: Vec<TerminalEvent>) -> (Vec<String>, Shared) {
    testutil::sandbox();
    let shared = Shared {
        core: Arc::new(Mutex::new(Core::new(crate::config::Config::default()))),
        chosen: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(Vec::new())),
        total: Arc::new(AtomicUsize::new(0)),
        menu_row: Arc::new(Mutex::new(None)),
    };
    let shared2 = shared.clone();
    let frames = smol::block_on(async move {
        let mut events = vec![resize()];
        events.extend(body);
        // Space events out so the 25ms poll loop + worker threads make progress.
        let stream = futures::stream::unfold(events.into_iter(), |mut it| async move {
            let ev = it.next()?;
            smol::Timer::after(Duration::from_millis(15)).await;
            Some((ev, it))
        });
        let mut elem = element! {
            ContextProvider(value: Context::owned(shared2)) {
                app::Palette
            }
        };
        elem.mock_terminal_render_loop(MockTerminalConfig::with_events(stream))
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .await
    });
    (frames, shared)
}

fn joined(frames: &[String]) -> String {
    frames.join("\n")
}

#[test]
fn command_mode_renders_and_fuzzy_matches() {
    let (frames, _) = run([typed(">tog"), ticks(4), vec![special(KeyCode::Esc)]].concat());
    let text = joined(&frames);
    assert!(text.contains("CMD"), "CMD mode pill should render");
    assert!(
        text.contains("Toggle"),
        "'>tog' should surface Toggle commands"
    );
}

#[test]
fn enter_records_the_selected_action() {
    // Type a query, move down one, activate. Chosen should be one of the toggles.
    let (_, shared) = run([
        typed(">tog"),
        ticks(4),
        vec![special(KeyCode::Down), special(KeyCode::Enter)],
    ]
    .concat());
    let chosen = shared.chosen.lock().unwrap().clone();
    let action = chosen.expect("Enter should record a chosen row").action;
    assert!(
        matches!(action, Action::ToggleSidebar | Action::TogglePanel),
        "expected a Toggle action, got {action:?}"
    );
}

#[test]
fn esc_dismisses_without_choosing() {
    let (_, shared) = run(vec![special(KeyCode::Esc)]);
    assert!(shared.chosen.lock().unwrap().is_none());
}

#[test]
fn tab_opens_and_closes_the_action_menu() {
    let (frames, _) = run([
        typed(">tog"),
        ticks(3),
        vec![special(KeyCode::Tab)], // open action menu
        ticks(3),
        vec![special(KeyCode::Esc)], // close menu (not the palette)
        ticks(2),
        vec![special(KeyCode::Esc)], // now dismiss
    ]
    .concat());
    let text = joined(&frames);
    assert!(text.contains("ACTIONS"), "Tab should open the ACTIONS menu");
}

#[test]
fn backspacing_past_the_prefix_returns_to_smart() {
    let (frames, _) = run([
        typed(">"),
        ticks(2),
        vec![special(KeyCode::Backspace)],
        ticks(3),
        vec![special(KeyCode::Esc)],
    ]
    .concat());
    // The final frame should be back in SMART mode.
    let last = frames.last().cloned().unwrap_or_default();
    assert!(
        last.contains("SMART"),
        "last frame should be SMART, got:\n{last}"
    );
}

#[test]
fn file_mode_streams_files() {
    // cwd is this crate's repo, so the walker finds Cargo.toml.
    let (frames, _) = run([typed("f Cargo"), ticks(20), vec![special(KeyCode::Esc)]].concat());
    let text = joined(&frames);
    assert!(text.contains("FILE"), "FILE mode pill should render");
    assert!(
        text.contains("Cargo.toml"),
        "file walker should surface Cargo.toml"
    );
}

#[test]
fn content_mode_streams_ripgrep_hits() {
    // Ticks must outlast the streaming ripgrep walk over the whole repo, which
    // grows with the file count — give it generous headroom so content.rs's hit
    // lands in a rendered frame before we snapshot.
    let (frames, _) = run([
        typed("/RegexMatcherBuilder"),
        ticks(60),
        vec![special(KeyCode::Esc)],
    ]
    .concat());
    let text = joined(&frames);
    assert!(text.contains("GREP"), "GREP mode pill should render");
    assert!(
        text.contains("content.rs"),
        "ripgrep should find the hit in content.rs"
    );
}

#[test]
fn nav_mode_lists_db_worktrees() {
    testutil::sandbox();
    let db = crate::db::Db::open().unwrap();
    db.put_worktree("r/feat-z", "/r", "/wt/feat-z", "feat/z", None)
        .unwrap();
    let (frames, _) = run([typed("@feat"), ticks(5), vec![special(KeyCode::Esc)]].concat());
    let text = joined(&frames);
    assert!(text.contains("NAV"), "NAV mode pill should render");
    assert!(text.contains("feat/z"), "nav should list the DB worktree");
}

#[test]
fn git_mode_shows_actions() {
    let (frames, _) = run([typed("g diff"), ticks(5), vec![special(KeyCode::Esc)]].concat());
    let text = joined(&frames);
    assert!(text.contains("GIT"), "GIT mode pill should render");
    assert!(
        text.contains("git diff"),
        "git mode should expose the diff action"
    );
}
