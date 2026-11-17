//! Approvals in the panel (b155 6a): a pinned group inside the existing
//! Notifications section (not a new `Section`). `y` approves, `n` rejects,
//! each behind the panel confirm; the panel calls `approvals.resolve` over
//! the UI's local control connection, off-loop. Without a daemon it says
//! "approvals need the daemon".
//!
//! Spine placeholder: b155 lane 5 fills it.
