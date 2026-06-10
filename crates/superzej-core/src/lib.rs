//! superzej-core — the substrate-agnostic heart of superzej.
//!
//! Everything here is independent of the UI substrate (zellij today, the native
//! host tomorrow): SQLite state, git/worktree/repo logic, the sandbox + remote
//! transport seams, config layering, the theme palette, and structured logging.
//! No module here references a multiplexer, a terminal emulator, or a renderer —
//! that is enforced by keeping `tokio`, `termwiz`, `iocraft`, and the native
//! service crates out of this crate's dependency set.

pub mod config;
pub mod db;
pub mod diff_highlight;
pub mod forge;
pub mod forgejo;
pub mod github;
pub mod keymap;
pub mod log;
pub mod models;
pub mod msg;
pub mod out;
pub mod picker;
pub mod plugin_api;
pub mod remote;
pub mod repo;
pub mod sandbox;
pub mod theme;
pub mod util;
pub mod worktree;
pub mod yazi;
