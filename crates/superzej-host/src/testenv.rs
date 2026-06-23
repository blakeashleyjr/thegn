//! Shared test-only synchronization for tests that mutate process-global state.
//!
//! Several modules' tests redirect `XDG_STATE_HOME` (via `set_var`) so the DB
//! opens against a throwaway dir. The process environment is global, so two such
//! tests in different modules — e.g. `run`'s sidebar-persistence tests and
//! `agent`'s sandbox tests — will clobber each other's `XDG_STATE_HOME` when the
//! test runner schedules them in parallel, unless they serialize on the *same*
//! lock. A per-module `static ENV_LOCK` does NOT do that (two mutexes, one
//! resource). This single crate-wide lock does.
//!
//! Hold it for the entire span between setting and restoring the env var:
//! `let _env = crate::testenv::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());`

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
