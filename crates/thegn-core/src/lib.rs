//! thegn-core — the substrate-agnostic heart of thegn.
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
pub mod activity;
// The activity FSM's pure transition; see the module's own docs. Public because
// it has two observers: the compositor's worktree-keyed `activity::poll` and the
// pane daemon's session-keyed `session_activity` — one decision function, so the
// two can never drift into disagreeing about what "quiet" means.
pub mod activity_step;
pub mod agent_task;
pub mod aggregate;
pub mod attention;
pub mod axis;
pub mod backoff;
pub mod blame;
pub mod bundle;
pub mod calendar;
pub mod capabilities;
pub mod capability;
pub mod capacity;
pub mod channel;
pub mod ci;
pub mod config;
pub mod config_activity;
pub mod config_calendar;
pub mod config_ci;
pub mod config_daemon;
pub mod config_defaults;
pub mod config_env_tables;
pub mod config_forge;
pub mod config_host_discovery;
pub mod config_issues;
pub mod config_loc;
pub mod config_media;
pub mod config_network;
pub mod config_observe;
pub mod config_placement;
pub mod config_pr_queue;
pub mod config_presets;
pub mod config_push;
pub mod config_remote;
pub mod config_resolve;
pub mod config_sandbox;
pub mod config_theme;
pub mod config_ui;
pub mod config_validate;
pub mod config_vpn;
pub mod config_write;
pub mod connectivity;
pub mod control;
pub mod control_wire;
pub mod custom_cmd;
pub mod db;
mod db_account;
mod db_aux;
mod db_cache;
mod db_calendar;
pub mod db_compute;
mod db_control;
mod db_glyph;
mod db_hibernate;
mod db_intent;
mod db_iroh;
pub mod db_migrate;
mod db_notification;
pub mod db_placement;
mod db_pool;
mod db_projects;
mod db_semantic;
mod db_trust;
mod db_usage;
mod db_workspace;
mod db_zones;
pub mod debug;
pub mod devcontainer;
pub mod devcontainer_features;
pub mod devcontainer_overlay;
pub mod devenv;
pub mod diff_highlight;
pub mod diff_sbs;
pub mod direnv;
pub mod disk;
pub mod dns_filter;
pub mod editor;
pub mod env;
pub mod envbuild;
pub mod envplan;
pub mod event_bus;
pub mod file_manager;
pub mod fold;
pub mod forge;
pub mod forward;
pub mod frecency;
pub mod fsperm;
pub mod gate;
pub mod github;
pub mod gitrefs;
pub mod gitviz;
pub mod grants;
// The one SSH host-key verification policy table (4 connection classes + one
// argv chokepoint). See the module docs; a shrink-only ratchet keeps host-key
// literals out of every other call site.
pub mod hostkey;
// Bounded, TTL'd holding pen for recently-dead things (the daemon's exited
// sessions), so a supervisor that polls a moment late still gets an answer.
pub mod diagnostics;
pub mod graveyard;
pub mod heal;
pub mod help;
pub mod history;
pub mod host;
pub mod host_config;
pub mod host_db;
pub mod host_machine;
pub mod host_probe;
pub mod i18n;
pub mod identity;
pub mod image;
pub mod inventory;
pub mod iroh_wire;
pub mod issue;
pub mod keymap;
pub mod layout_import;
pub mod lifecycle;
pub mod loc;
pub mod log;
pub mod log_redact;
pub mod log_trace;
pub mod log_view;
pub mod lsp_registry;
pub mod managed_tool;
pub mod mcp;
pub mod media;
pub mod merge_guard;
pub mod merge_lifecycle;
pub mod merge_sweep;
pub mod metrics;
pub mod migrate_brand;
pub mod models;
pub mod msg;
pub mod notification;
pub mod notification_route;
pub mod notify_debounce;
pub mod push_inbox;
// `OSC 9` / `OSC 777` attention signalling: how a process says "I need you"
// instead of thegn guessing from CPU and silence.
pub mod osc_attention;
pub mod out;
// Bounded compilation of caller-supplied `wait --until match:<regex>` patterns.
pub mod output_match;
pub mod paste_drop;
pub mod patch;
pub mod picker;
pub mod placement;
#[cfg(test)]
mod platform_ratchet_tests;
pub mod plugin_api;
pub mod pr_queue;
pub mod preview;
pub mod proc_registry;
pub mod profile;
pub mod progress;
pub mod project;
pub mod projection;
pub mod pull_progress;
pub mod rebase_todo;
// The canonical secret-redaction seam: one sensitive-key predicate + JSON
// masker shared by every leak surface (MCP docs, crash reporter, doctor, the
// typed SecretRef). See the module docs — new surfaces import from here.
pub mod redact;
pub mod reflog;
pub mod registers;
pub mod remote;
pub mod remote_tune;
pub mod repo;
pub mod repo_trust;
pub mod resource_alert;
pub mod retry;
pub mod revtunnel;
pub mod sandbox;
pub mod sandbox_backend;
pub mod sandbox_build;
pub mod sandbox_compose;
pub mod sandbox_cpucap;
pub mod sandbox_dormant;
pub mod sandbox_mountcheck;
pub mod sandbox_mounts;
pub mod sandbox_prefetch;
pub mod sandbox_preflight;
pub mod sandbox_runtime;
pub mod sandbox_support;
pub mod sandbox_truth;
pub mod scan_sched;
pub mod scheduler;
pub mod seam;
pub mod search;
// The value-free secret audit trail (target `thegn::secret::audit`).
pub mod secret_audit;
// Enumerate every configured secret reference in a Config (one source for the
// CLI, `config validate`'s plaintext warning, and doctor's presence rows).
pub mod secret_scan;
// The SecretStore provider seam (keyring/file/env impl, exec reserved).
pub mod secret_store;
// The typed secret-reference vocabulary (keyring/env/file/literal), parsed once
// at config load; redacted Debug, no Display/Serialize of a literal value.
pub mod search_replace;
pub mod secretref;
pub mod semantic;
pub mod semantic_graph;
pub mod series;
pub mod series_window;
// The pane daemon's per-session observer of the activity FSM (one decision
// function, two observers -- see the module docs).
pub mod session_activity;
pub mod share;
pub mod shellinv;
pub mod snapshot_meta;
pub mod spillover;
pub mod ssh_creds;
pub mod startup;
pub mod store;
pub mod syncstate;
pub mod tailnet;
pub mod term_snapshot;
pub mod termcaps;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;
// Public under `test-utils` so dependent crates get the SAME guard rather than
// growing their own — `testenv`'s whole point is one mutex per resource, and a
// per-crate copy is how that invariant erodes. Compiled out of a normal build.
#[cfg(any(test, feature = "test-utils"))]
pub mod testenv;
pub mod theme;
pub mod theme_contrast;
pub mod toolchain;
pub mod transport_error;
pub mod trust_class;
pub mod usage;
pub mod usage_alert;
pub mod usage_tokens;
pub mod util;
pub mod viz;
pub mod work;
pub mod worktree;
pub mod zone;
