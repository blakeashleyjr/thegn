//! Provider-agnostic media-player model — re-exported from the `superzej-media`
//! leaf crate so existing `superzej_core::media::*` paths keep working.
//!
//! The types moved out of core into a C-dep-free leaf so the per-OS control
//! backends (MPRIS / SMTC / mpv / AppleScript) and their model can be
//! `cargo check --target`-ed for macOS + Windows on a Linux box (the leaf can't
//! depend on core, which compiles C via rusqlite/tree-sitter). Config stays
//! here; see [`crate::config::MediaConfig::resolve_opts`] for the lowering into
//! the leaf's `ResolveOpts`.

pub use superzej_media::model::*;
