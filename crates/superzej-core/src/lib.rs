//! superzej-core — the substrate-agnostic heart of superzej.
//!
//! Everything here is independent of the UI substrate (the native host): SQLite
//! state, git/worktree/repo logic, the sandbox + remote transport seams, config
//! layering, the theme palette, and structured logging. No module here references
//! a multiplexer, a terminal emulator, or a renderer — that is enforced by keeping
//! `tokio`, `termwiz`, and the native service crates out of this crate's
//! dependency set. The `keymap` module is the keybinding *registry* (effective
//! bindings + collision detection for the cheatsheet/`keys validate`); the host
//! owns terminal chord→Action routing.

pub mod activity;
pub mod config;
pub mod custom_cmd;
pub mod db;
pub mod diff_highlight;
pub mod diff_sbs;
pub mod forge;
pub mod github;
pub mod gitrefs;
pub mod gitviz;
pub mod keymap;
pub mod log;
pub mod models;
pub mod msg;
pub mod out;
pub mod patch;
pub mod picker;
pub mod plugin_api;
pub mod rebase_todo;
pub mod reflog;
pub mod remote;
pub mod repo;
pub mod sandbox;
pub mod theme;
pub mod util;
pub mod viz;
pub mod worktree;
pub mod yazi;
