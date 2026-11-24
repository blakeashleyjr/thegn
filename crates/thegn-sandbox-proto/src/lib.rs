//! Portable wire types shared by `thegn-core` and the Windows sandbox helper.
//!
//! The helper (`thegn-sandbox-win.exe`) must not depend on `thegn-core`, and
//! core must be able to lower Windows sandbox policies to a shape the helper
//! decodes, so both sides depend on this crate instead. It is deliberately
//! serde-only: no `cfg` code, no spawn, no threads, no OS calls — which is also
//! why it is exempt from the per-crate platform, spawn-ownership and
//! thread-name scans (see the crate README).
//!
//! [`win`] holds the versioned request, probe and status types plus the bounded
//! environment transport; [`win_cmdline`] holds the argv quoting both sides must
//! agree on.

#![forbid(unsafe_code)]

pub mod win;
pub mod win_cmdline;
