use std::panic;
use sz_kit::input::{InputEvent, InputResult};
use sz_kit::ratatui::buffer::Buffer;
use sz_kit::ratatui::layout::Rect;
use sz_kit::tile::{AppTile, ChangeHook};

use chrono::Utc;
use gtui_app::app::ObserveApp;
use gtui_core::datasource::TimeRange;

pub struct ObserveTile {
    app: ObserveApp,
    is_panicked: bool,
    needs_redraw: bool,
}

impl ObserveTile {
    pub fn new(waker: ChangeHook) -> Self {
        let (app, _tx) = ObserveApp::new(
            TimeRange {
                from: Utc::now(),
                to: Utc::now(),
            },
            waker,
        );
        Self {
            app,
            is_panicked: false,
            needs_redraw: true,
        }
    }
}

impl AppTile for ObserveTile {
    fn id(&self) -> &'static str {
        "observe"
    }

    fn title(&self) -> String {
        if self.is_panicked {
            "Observe (Crashed)".to_string()
        } else {
            "Observe".to_string()
        }
    }

    fn pump(&mut self) -> bool {
        if self.is_panicked {
            return false;
        }

        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            if self.app.tick().is_some() {
                self.needs_redraw = true;
                true
            } else {
                false
            }
        }));

        match result {
            Ok(changed) => changed,
            Err(_) => {
                self.is_panicked = true;
                self.needs_redraw = true;
                true
            }
        }
    }

    fn wants_redraw(&self) -> bool {
        self.needs_redraw
    }

    fn handle_input(&mut self, _event: InputEvent) -> InputResult {
        if self.is_panicked {
            return InputResult::Ignored;
        }
        // Not handling real input yet, just say we consumed it
        InputResult::Consumed
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.needs_redraw = false;

        if self.is_panicked {
            use sz_kit::ratatui::style::{Color, Style};
            use sz_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget};
            let w = Paragraph::new("The Observe panel crashed. Host remains healthy.")
                .style(Style::default().fg(Color::Red))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Observe (Panic)"),
                );
            w.render(area, buf);
            return;
        }

        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            // Render the actual app. For now we just draw a placeholder block.
            use sz_kit::ratatui::widgets::{Block, Borders, Widget};
            Block::default()
                .borders(Borders::ALL)
                .title("Observe")
                .render(area, buf);
        }));

        if result.is_err() {
            self.is_panicked = true;
            // Next frame it will render the panic text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use sz_kit::input::Key;

    #[test]
    fn test_observe_tile_panic_boundary() {
        let waker = Arc::new(|| {});
        let mut tile = ObserveTile::new(waker);

        assert_eq!(tile.title(), "Observe");
        assert!(!tile.is_panicked);

        // Simulating a panic by force-setting the flag since we can't easily
        // inject a panic without mocking the app internals more deeply.
        tile.is_panicked = true;

        assert_eq!(tile.title(), "Observe (Crashed)");
        // Update should return early
        assert_eq!(
            tile.handle_input(InputEvent::key(Key::Char('a'))),
            InputResult::Ignored
        );
    }
}
