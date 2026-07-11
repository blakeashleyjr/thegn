//! `thegn-agent` — the lean in-sandbox dialer, as a library.
//!
//! The `tg-agent` binary (`main.rs`) is a thin wrapper over [`serve`]; exposing
//! the serving logic as a lib lets the transport be proven in-process against
//! `thegn-svc::iroh_reach` (home side) in an integration test without a real
//! remote. See `tests/pty_over_iroh.rs`.

pub mod serve;
