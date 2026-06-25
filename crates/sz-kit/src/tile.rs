//! [`AppTile`] — the contract the superzej host (and the standalone harness)
//! drive every frame.
//!
//! The trait is the *drive* surface, not a factory: each app keeps its own
//! `new(deps…, rt, on_change, theme)` constructor because clients and config
//! differ. The discipline that keeps superzej at ~0% idle is encoded here —
//! [`AppTile::handle_input`] must never block or await, and async results land
//! via a [`ChangeHook`] that wakes the host loop instead of polling.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::input::{InputEvent, InputResult};

/// Fired from a tile's async side to tell the host "I have new state, pump me
/// and redraw." Embedded: superzej pulses its `TerminalWaker`. Standalone: the
/// harness posts a wake-up on its loop channel. Cheap and idempotent — fire it
/// liberally; the host coalesces.
pub type ChangeHook = std::sync::Arc<dyn Fn() + Send + Sync>;

/// A full TUI application embeddable as a superzej app tab.
///
/// Not `Send`: the host drives tiles on its single event-loop thread (the loop
/// future is `block_on`'d, never `spawn`ed) and the standalone harness drives
/// them on its main thread, so tiles never cross threads — and some ratatui
/// 0.30 widgets a tile may hold (e.g. a `Block` with a shadow `Effect`) are
/// themselves `!Send`. Async work is offloaded via the `ChangeHook` instead.
pub trait AppTile {
    /// Stable identifier — the tab id and config key.
    fn id(&self) -> &'static str;

    /// The chip label shown in the masthead app strip. May carry a badge
    /// (e.g. an unread count) that the host re-reads after each [`pump`].
    ///
    /// [`pump`]: AppTile::pump
    fn title(&self) -> String;

    /// Fold any async results that arrived since the last call into view
    /// state. Returns `true` if anything changed (the host should redraw).
    /// Must not block.
    fn pump(&mut self) -> bool;

    /// Whether the tile needs a redraw. The host re-runs [`render`] only when
    /// this is `true`; otherwise it re-blits the cached buffer.
    ///
    /// [`render`]: AppTile::render
    fn wants_redraw(&self) -> bool;

    /// Handle one input event. Never blocks or awaits — kick async work onto a
    /// task that reports back through the [`ChangeHook`].
    fn handle_input(&mut self, ev: InputEvent) -> InputResult;

    /// Render the current view into `area` of `buf`. Pure and synchronous.
    fn render(&mut self, area: Rect, buf: &mut Buffer);

    /// The text-input caret position, relative to the tile's `area`, if the
    /// tile wants a visible hardware cursor. `None` hides it.
    fn cursor(&self) -> Option<(u16, u16)> {
        None
    }

    /// A contribution to the host statusbar while this tile is focused.
    fn status_line(&self) -> Option<String> {
        None
    }

    /// Focus gained/lost. Default: ignore.
    fn on_focus(&mut self, _focused: bool) {}

    /// The tile's draw area changed size. Default: ignore (most tiles relayout
    /// from `area` in [`render`]).
    ///
    /// [`render`]: AppTile::render
    fn on_resize(&mut self, _cols: u16, _rows: u16) {}

    /// Tear down: abort spawned tasks and drop clients. Long-lived external
    /// daemons an app may talk to are intentionally NOT killed here.
    fn shutdown(&mut self) {}
}
