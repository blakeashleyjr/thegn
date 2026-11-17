//! The programme's one egress vocabulary (resolutions B11), stubbed by
//! WS00-P1-03 so every workstream compiles against the same names.
//!
//! Submodules and their owners:
//! - [`plan`]: the egress plan a sandbox launch carries (`SandboxSpec.egress`);
//!   owned by WS04-P2-08 after WS00.
//!
//! Later items add their own submodules here: WS01-P2-01 adds `pattern` (host
//! patterns, today's semantics) and WS04 adds `policy`, `preset`, `resolve` and
//! `wire`. The reporting enum [`EgressKind`](crate::capabilities::EgressKind)
//! stays in `capabilities`; its semantics are owned by WS01-P1-02.
//!
//! Nothing here carries behaviour yet: no constructor, no caller, no I/O.

pub mod plan;
