//! The standalone crossterm harness (feature `standalone`).
//!
//! Each app's `main` builds a tokio runtime, then hands a tile constructor to
//! [`run`]; this owns the terminal, the input thread, and the event loop. It
//! is **event-driven, not polled**: a dedicated thread does blocking
//! `crossterm::event::read()`, the tile's [`ChangeHook`] posts a wake-up on the
//! same channel, and the main thread blocks on `recv()` — so a standalone app
//! sits at ~0% idle just like it does embedded in thegn.

use std::io::{self, Stdout};
use std::sync::mpsc;

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

/// Run `build`'s tile to completion as a standalone TUI. `build` receives the
/// [`ChangeHook`] to wire into the tile's async side. Returns when the tile
/// signals [`InputResult::Exit`] or input ends.
pub fn run(
    build: impl FnOnce(ChangeHook) -> anyhow::Result<Box<dyn AppTile>>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<Wake>();

    let hook_tx = tx.clone();
    let hook: ChangeHook = std::sync::Arc::new(move || {
        // A closed channel just means we're shutting down; ignore.
        let _ = hook_tx.send(Wake::Pump);
    });

    let mut tile = build(hook)?;

    // Blocking input reader — the only thread that touches stdin.
    let in_tx = tx;
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if in_tx.send(Wake::Event(ev)).is_err() {
                break; // loop gone
            }
        }
    });

    let mut term = setup_terminal()?;
    let result = event_loop(tile.as_mut(), &mut term, &rx);
    restore_terminal(&mut term)?;
    tile.shutdown();
    result
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

fn setup_terminal() -> anyhow::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

    // Restore the terminal even if the tile panics, so the shell stays usable.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
        prev(info);
    }));

    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(term: &mut Term) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
    term.show_cursor()?;
    Ok(())
}
