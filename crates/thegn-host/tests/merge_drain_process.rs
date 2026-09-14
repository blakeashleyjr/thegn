//! Actual private merge-drain acceptance. Linux custody is explicit; no native
//! Windows/macOS claim and no real provider, remote or user queue is involved.
#[cfg(target_os = "linux")]
#[path = "merge_drain_support/linux.rs"]
mod linux;
