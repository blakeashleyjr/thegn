//! The standalone crossterm harness (feature `standalone`).
//!
//! Each app's `main` builds a tokio runtime, then hands a tile constructor to
//! [`run`]; this owns the terminal lifecycle and event loop. It is
//! **event-driven, not polled**: a dedicated thread does blocking
//! `crossterm::event::read()`, the tile's [`ChangeHook`] posts a wake-up on the
//! same channel, and the main thread blocks on `recv()` — so a standalone app
//! sits at ~0% idle just like it does embedded in thegn. One reader is reused
//! per process. It can consume and park at most one event after the last run
//! returns; blocking `read()` cannot be cancelled without crossterm's
//! `event-stream` feature (polling would break the ~0%-idle contract).

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex, Once};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::input::{InputEvent, InputResult, from_crossterm_event};
use crate::tile::{AppTile, ChangeHook};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// One thing the loop wakes for: a terminal event, or a tile change-hook pulse.
enum Wake {
    Event(Event),
    Pump,
}

const PENDING_EVENT_CAPACITY: usize = 16;

#[derive(Default)]
struct InputRouter {
    sink: Option<mpsc::Sender<Wake>>,
    reader_alive: bool,
    pending: VecDeque<Event>,
}

struct Attach {
    spawn_reader: bool,
    replay: Vec<Event>,
}

#[derive(Debug, PartialEq, Eq)]
struct AlreadyActive;

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Continue,
    Exit,
}

impl InputRouter {
    fn attach(&mut self, sink: mpsc::Sender<Wake>) -> Result<Attach, AlreadyActive> {
        if self.sink.is_some() {
            return Err(AlreadyActive);
        }
        self.sink = Some(sink);
        let replay = self.pending.drain(..).collect();
        let spawn_reader = !self.reader_alive;
        self.reader_alive = true;
        Ok(Attach {
            spawn_reader,
            replay,
        })
    }

    fn detach(&mut self) {
        self.sink = None;
    }

    fn route(&mut self, event: Event) -> Route {
        if let Some(sink) = &self.sink {
            if let Err(mpsc::SendError(Wake::Event(event))) = sink.send(Wake::Event(event)) {
                self.park(event);
                self.sink = None;
                self.reader_alive = false;
                return Route::Exit;
            }
            return Route::Continue;
        }

        self.park(event);
        self.reader_alive = false;
        Route::Exit
    }

    fn reader_stopped(&mut self) {
        self.reader_alive = false;
    }

    fn reader_spawn_finished(&mut self, should_spawn: bool, spawned: bool) {
        if should_spawn && !spawned {
            self.reader_stopped();
        }
    }

    fn park(&mut self, event: Event) {
        if self.pending.len() < PENDING_EVENT_CAPACITY {
            self.pending.push_back(event);
        }
    }
}

static INPUT: LazyLock<Mutex<InputRouter>> = LazyLock::new(|| Mutex::new(InputRouter::default()));
static HARNESS_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

struct InputAttachment;

impl Drop for InputAttachment {
    fn drop(&mut self) {
        let mut input = INPUT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        input.detach();
        HARNESS_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Run `build`'s tile to completion as a standalone TUI. `build` receives the
/// [`ChangeHook`] to wire into the tile's async side. Returns when the tile
/// signals [`InputResult::Exit`] or input ends.
pub fn run(
    build: impl FnOnce(ChangeHook) -> anyhow::Result<Box<dyn AppTile>>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<Wake>();

    let Attach {
        spawn_reader,
        replay,
    } = {
        let mut input = INPUT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let attached = input
            .attach(tx.clone())
            .map_err(|_| anyhow::anyhow!("standalone harness already running"))?;
        HARNESS_ACTIVE.store(true, Ordering::SeqCst);
        attached
    };
    let _attachment = InputAttachment;

    for event in replay {
        if let Err(_error) = tx.send(Wake::Event(event)) {
            // The fresh receiver is alive here; keep a defensive best-effort boundary.
            break;
        }
    }

    install_panic_hook_once();

    let hook_tx = tx.clone();
    let hook: ChangeHook = std::sync::Arc::new(move || {
        // A closed channel just means we're shutting down; ignore.
        if let Err(_error) = hook_tx.send(Wake::Pump) {
            // best-effort: a closed channel means the harness is shutting down.
        }
    });

    let mut spawned = false;
    let result = (|| {
        let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let tile = build(hook)?;

        let mut ops = CrosstermTerminalOps;
        run_lifecycle(&mut ops, tile, |tile| {
            // Start blocking input only after terminal setup succeeded.
            if spawn_reader {
                std::thread::Builder::new()
                    .name("tg-kit-input".into())
                    .spawn(input_reader)?;
                spawned = true;
            }
            event_loop(tile, &mut term, &rx)
        })
    })();

    let mut input = INPUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    input.reader_spawn_finished(spawn_reader, spawned);
    result
}

fn input_reader() {
    loop {
        match event::read() {
            Ok(event) => {
                let route = {
                    let mut input = INPUT
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    input.route(event)
                };
                if route == Route::Exit {
                    return;
                }
            }
            Err(_) => {
                let mut input = INPUT
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                input.reader_stopped();
                return;
            }
        }
    }
}

fn install_panic_hook_once() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if HARNESS_ACTIVE.load(Ordering::SeqCst) {
                if let Err(_error) = disable_raw_mode() {
                    // best-effort: panic reporting must not mask the panic.
                }
                if let Err(_error) =
                    execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste)
                {
                    // best-effort: panic reporting must not mask the panic.
                }
            }
            previous(info);
        }));
    });
}

fn event_loop(
    tile: &mut dyn AppTile,
    term: &mut Term,
    rx: &mpsc::Receiver<Wake>,
) -> anyhow::Result<()> {
    draw(tile, term)?;
    while let Ok(wake) = rx.recv() {
        let mut dirty = false;
        match wake {
            Wake::Pump => {
                dirty |= tile.pump();
            }
            Wake::Event(ev) => match from_crossterm_event(ev) {
                Some(InputEvent::Resize(cols, rows)) => {
                    tile.on_resize(cols, rows);
                    dirty = true;
                }
                Some(input) => match tile.handle_input(input) {
                    InputResult::Exit => return Ok(()),
                    InputResult::Consumed => dirty = true,
                    InputResult::Ignored => {}
                },
                None => {}
            },
        }
        // Fold anything the change-hook delivered alongside, then redraw once.
        dirty |= tile.pump();
        if dirty || tile.wants_redraw() {
            draw(tile, term)?;
        }
    }
    Ok(())
}

fn draw(tile: &mut dyn AppTile, term: &mut Term) -> anyhow::Result<()> {
    term.draw(|frame| {
        let area = frame.area();
        tile.render(area, frame.buffer_mut());
        if let Some((cx, cy)) = tile.cursor() {
            frame.set_cursor_position((area.x.saturating_add(cx), area.y.saturating_add(cy)));
        }
    })?;
    Ok(())
}

trait TerminalOps {
    fn enable_raw(&mut self) -> anyhow::Result<()>;
    fn disable_raw(&mut self) -> anyhow::Result<()>;
    fn enter_alternate(&mut self) -> anyhow::Result<()>;
    fn leave_alternate(&mut self) -> anyhow::Result<()>;
    fn enable_paste(&mut self) -> anyhow::Result<()>;
    fn disable_paste(&mut self) -> anyhow::Result<()>;
    fn show_cursor(&mut self) -> anyhow::Result<()>;
}

struct CrosstermTerminalOps;

impl TerminalOps for CrosstermTerminalOps {
    fn enable_raw(&mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        Ok(())
    }

    fn disable_raw(&mut self) -> anyhow::Result<()> {
        disable_raw_mode()?;
        Ok(())
    }

    fn enter_alternate(&mut self) -> anyhow::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(())
    }

    fn leave_alternate(&mut self) -> anyhow::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)?;
        Ok(())
    }

    fn enable_paste(&mut self) -> anyhow::Result<()> {
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(())
    }

    fn disable_paste(&mut self) -> anyhow::Result<()> {
        execute!(io::stdout(), DisableBracketedPaste)?;
        Ok(())
    }

    fn show_cursor(&mut self) -> anyhow::Result<()> {
        execute!(io::stdout(), ratatui::crossterm::cursor::Show)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum TerminalMode {
    Raw,
    Alternate,
    Paste,
}

struct TerminalGuard<'a, O: TerminalOps> {
    ops: &'a mut O,
    applied: Vec<TerminalMode>,
    finished: bool,
}

impl<'a, O: TerminalOps> TerminalGuard<'a, O> {
    fn new(ops: &'a mut O) -> Self {
        Self {
            ops,
            applied: Vec::new(),
            finished: false,
        }
    }

    fn setup(&mut self) -> anyhow::Result<()> {
        self.ops.enable_raw()?;
        self.applied.push(TerminalMode::Raw);
        self.ops.enter_alternate()?;
        self.applied.push(TerminalMode::Alternate);
        self.ops.enable_paste()?;
        self.applied.push(TerminalMode::Paste);
        Ok(())
    }

    fn finish(&mut self) -> Vec<anyhow::Error> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut errors = Vec::new();
        while let Some(mode) = self.applied.pop() {
            let result = match mode {
                TerminalMode::Raw => self.ops.disable_raw(),
                TerminalMode::Alternate => self.ops.leave_alternate(),
                TerminalMode::Paste => self.ops.disable_paste(),
            };
            if let Err(error) = result {
                errors.push(error);
            }
        }
        if let Err(error) = self.ops.show_cursor() {
            errors.push(error);
        }
        errors
    }
}

impl<O: TerminalOps> Drop for TerminalGuard<'_, O> {
    fn drop(&mut self) {
        drop(self.finish());
    }
}

struct TileGuard {
    tile: Box<dyn AppTile>,
    finished: bool,
}

impl TileGuard {
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.tile.shutdown();
        }
    }
}

impl Drop for TileGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Debug)]
struct LifecycleError {
    primary: Option<anyhow::Error>,
    teardown: Vec<anyhow::Error>,
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(primary) = &self.primary {
            write!(f, "{primary:#}")?;
        } else {
            write!(f, "standalone teardown failed")?;
        }
        for error in &self.teardown {
            write!(f, "; teardown: {error:#}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

fn run_lifecycle<O: TerminalOps>(
    ops: &mut O,
    tile: Box<dyn AppTile>,
    body: impl FnOnce(&mut dyn AppTile) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut tile = TileGuard {
        tile,
        finished: false,
    };
    let mut terminal = TerminalGuard::new(ops);
    let primary = match terminal.setup() {
        Ok(()) => body(tile.tile.as_mut()),
        Err(error) => Err(error),
    };
    let teardown = terminal.finish();
    tile.finish();
    if primary.is_err() || !teardown.is_empty() {
        return Err(LifecycleError {
            primary: primary.err(),
            teardown,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn key_a() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
    }

    #[derive(Default)]
    struct FakeTerminal {
        calls: Vec<&'static str>,
        fail: Option<&'static str>,
    }

    impl FakeTerminal {
        fn step(&mut self, name: &'static str) -> anyhow::Result<()> {
            self.calls.push(name);
            if self.fail == Some(name) {
                anyhow::bail!("failed {name}");
            }
            Ok(())
        }
    }

    impl TerminalOps for FakeTerminal {
        fn enable_raw(&mut self) -> anyhow::Result<()> {
            self.step("raw+")
        }
        fn disable_raw(&mut self) -> anyhow::Result<()> {
            self.step("raw-")
        }
        fn enter_alternate(&mut self) -> anyhow::Result<()> {
            self.step("alt+")
        }
        fn leave_alternate(&mut self) -> anyhow::Result<()> {
            self.step("alt-")
        }
        fn enable_paste(&mut self) -> anyhow::Result<()> {
            self.step("paste+")
        }
        fn disable_paste(&mut self) -> anyhow::Result<()> {
            self.step("paste-")
        }
        fn show_cursor(&mut self) -> anyhow::Result<()> {
            self.step("cursor+")
        }
    }

    struct SpyTile(Arc<AtomicUsize>);

    impl AppTile for SpyTile {
        fn id(&self) -> &'static str {
            "spy"
        }
        fn title(&self) -> String {
            "spy".to_owned()
        }
        fn pump(&mut self) -> bool {
            false
        }
        fn wants_redraw(&self) -> bool {
            false
        }
        fn handle_input(&mut self, _: InputEvent) -> InputResult {
            InputResult::Ignored
        }
        fn render(&mut self, _: Rect, _: &mut Buffer) {}
        fn shutdown(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn spy() -> (Box<dyn AppTile>, Arc<AtomicUsize>) {
        let shutdowns = Arc::new(AtomicUsize::new(0));
        (Box::new(SpyTile(shutdowns.clone())), shutdowns)
    }

    #[test]
    fn setup_failures_restore_only_applied_modes_and_shutdown_once() {
        for (failure, expected) in [
            ("raw+", vec!["raw+", "cursor+"]),
            ("alt+", vec!["raw+", "alt+", "raw-", "cursor+"]),
            (
                "paste+",
                vec!["raw+", "alt+", "paste+", "alt-", "raw-", "cursor+"],
            ),
        ] {
            let (tile, shutdowns) = spy();
            let mut terminal = FakeTerminal {
                calls: Vec::new(),
                fail: Some(failure),
            };
            let result = run_lifecycle(&mut terminal, tile, |_| Ok(()));
            assert!(result.is_err());
            assert_eq!(terminal.calls, expected);
            assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn loop_error_survives_restore_failure_and_shutdown_runs_once() {
        let (tile, shutdowns) = spy();
        let mut terminal = FakeTerminal {
            calls: Vec::new(),
            fail: Some("alt-"),
        };
        let result = run_lifecycle(&mut terminal, tile, |_| anyhow::bail!("body failed"));
        let error = result.unwrap_err().to_string();
        assert!(error.contains("body failed"));
        assert!(error.contains("failed alt-"));
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(
            terminal.calls,
            [
                "raw+", "alt+", "paste+", "paste-", "alt-", "raw-", "cursor+"
            ]
        );
    }

    #[test]
    fn lifecycle_error_displays_primary_context_once() {
        let primary = anyhow::anyhow!("root cause").context("body failed");
        let error = LifecycleError {
            primary: Some(primary),
            teardown: Vec::new(),
        };

        let displayed = error.to_string();
        assert!(displayed.contains("body failed: root cause"));
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn every_restore_failure_still_attempts_remaining_modes() {
        for failure in ["paste-", "alt-", "raw-", "cursor+"] {
            let (tile, shutdowns) = spy();
            let mut terminal = FakeTerminal {
                calls: Vec::new(),
                fail: Some(failure),
            };
            let result = run_lifecycle(&mut terminal, tile, |_| Ok(()));
            assert!(result.is_err(), "{failure} should be reported");
            assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
            assert_eq!(
                terminal.calls,
                [
                    "raw+", "alt+", "paste+", "paste-", "alt-", "raw-", "cursor+"
                ]
            );
        }
    }

    #[test]
    fn unwind_restores_modes_and_shuts_down_once() {
        let (tile, shutdowns) = spy();
        let mut terminal = FakeTerminal::default();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(run_lifecycle(&mut terminal, tile, |_| panic!("body panic")).is_ok());
        }));
        assert!(panic.is_err());
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(
            terminal.calls,
            [
                "raw+", "alt+", "paste+", "paste-", "alt-", "raw-", "cursor+"
            ]
        );
    }

    #[test]
    fn sequential_runs_return_terminal_to_baseline() {
        let mut terminal = FakeTerminal::default();
        for _ in 0..2 {
            let (tile, shutdowns) = spy();
            run_lifecycle(&mut terminal, tile, |_| Ok(())).unwrap();
            assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            terminal.calls,
            [
                "raw+", "alt+", "paste+", "paste-", "alt-", "raw-", "cursor+", "raw+", "alt+",
                "paste+", "paste-", "alt-", "raw-", "cursor+",
            ]
        );
    }

    #[test]
    fn sequential_runs_reuse_reader_and_replay_parked_event() {
        let mut input = InputRouter::default();
        let (tx, _rx) = mpsc::channel();
        let first = input.attach(tx).unwrap();
        assert!(first.spawn_reader);
        assert!(first.replay.is_empty());

        input.detach();
        assert_eq!(input.route(key_a()), Route::Exit);

        let (tx, _rx) = mpsc::channel();
        let second = input.attach(tx).unwrap();
        assert!(second.spawn_reader);
        assert_eq!(second.replay, [key_a()]);

        input.detach();
        let (tx, _rx) = mpsc::channel();
        let third = input.attach(tx).unwrap();
        assert!(!third.spawn_reader);
        assert!(third.replay.is_empty());
    }

    #[test]
    fn concurrent_attach_is_refused() {
        let mut input = InputRouter::default();
        let (tx, _rx) = mpsc::channel();
        input.attach(tx).unwrap();
        let (tx, _rx) = mpsc::channel();
        assert!(matches!(input.attach(tx), Err(AlreadyActive)));
    }

    #[test]
    fn route_to_dropped_receiver_parks_instead_of_losing() {
        let mut input = InputRouter::default();
        let (tx, rx) = mpsc::channel();
        input.attach(tx).unwrap();
        drop(rx);

        assert_eq!(input.route(key_a()), Route::Exit);
        assert_eq!(input.pending, [key_a()]);
        assert!(input.sink.is_none());
        assert!(!input.reader_alive);
    }

    #[test]
    fn pending_is_bounded() {
        let mut input = InputRouter::default();
        for _ in 0..=PENDING_EVENT_CAPACITY {
            input.park(key_a());
        }
        assert_eq!(input.pending.len(), PENDING_EVENT_CAPACITY);
    }

    #[test]
    fn failed_reader_spawn_allows_next_run_to_spawn() {
        let mut input = InputRouter::default();
        let (tx, _rx) = mpsc::channel();
        let first = input.attach(tx).unwrap();
        assert!(first.spawn_reader);

        input.reader_spawn_finished(first.spawn_reader, false);
        input.detach();

        let (tx, _rx) = mpsc::channel();
        let second = input.attach(tx).unwrap();
        assert!(second.spawn_reader);
    }
}
