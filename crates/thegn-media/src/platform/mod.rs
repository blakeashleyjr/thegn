//! Per-operating-system media implementations.
//!
//! Platform selection belongs at this module boundary so the leaf's pure model
//! and provider seam remain cross-target compilable without leaking D-Bus,
//! WinRT, or AppleScript details into callers.

#[cfg(target_os = "linux")]
pub mod linux;
