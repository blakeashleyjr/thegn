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

pub mod account;
pub mod acp;
pub mod activity;
pub mod blame;
pub mod ci;
pub mod config;
pub mod custom_cmd;
pub mod db;
pub mod devenv;
pub mod diff_highlight;
pub mod diff_sbs;
pub mod disk;
pub mod dns_filter;
pub mod env;
pub mod event_bus;
pub mod forge;
pub mod github;
pub mod gitrefs;
pub mod gitviz;
pub mod history;
pub mod i18n;
pub mod issue;
pub mod keymap;
pub mod log;
pub mod log_trace;
pub mod log_view;
pub mod mcp;
pub mod media;
pub mod metrics;
pub mod models;
pub mod msg;
pub mod notification;
pub mod out;
pub mod patch;
pub mod picker;
pub mod placement;
pub mod plugin_api;
pub mod proxy;
pub mod rebase_todo;
pub mod reflog;
pub mod remote;
pub mod repo;
pub mod sandbox;
pub mod search;
pub mod semantic;
pub mod share;
pub mod startup;
pub mod theme;
pub mod util;
pub mod viz;
pub mod work;
pub mod worktree;
pub mod yazi;
