//! `sz-kit` — the superzej embedding contract.
//!
//! Sibling TUIs (termite-chat/`chat`, termite-agent/`agent`) plug into the
//! superzej compositor as top-level
//! **app tabs**, while each still ships as a standalone binary. This crate is
//! the seam they share so the look, input model, and event discipline stay
//! identical in both modes — and it deliberately depends on neither tokio,
//! termwiz, nor `superzej-core`, so a standalone app can link it without
//! dragging in superzej's stack.
//!
//! - [`AppTile`] — the drive contract the host calls each frame.
//! - [`InputEvent`] / [`Key`] / [`Modifiers`] — backend-agnostic input; the
//!   host translates termwiz into these, the standalone harness translates
//!   crossterm.
//! - [`Theme`] — semantic tokens → sRGB, with [`Theme::prism`] defaults that
//!   mirror superzej's chrome palette and a tolerant config loader so a
//!   standalone binary picks up the user's superzej theme.
//! - [`standalone::run`] (feature `standalone`) — a ~zero-idle crossterm loop
//!   that shrinks each app's `main` to a few lines.
//!
//! ## ratatui version pinning
//!
//! [`ratatui`] is re-exported here. The host and every app MUST render through
//! `sz_kit::ratatui` (never a direct `ratatui` dep at a different version), so
//! the shared [`ratatui::buffer::Buffer`] is one type everywhere and drift is a
//! compile error rather than a silent mis-render.

pub use ratatui;

pub mod input;
pub mod theme;
pub mod tile;

#[cfg(feature = "standalone")]
pub mod standalone;

pub use input::{InputEvent, InputResult, Key, Modifiers};
pub use theme::{Rgb, Theme, Tok};
pub use tile::{AppTile, ChangeHook};
