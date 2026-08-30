//! Event-driven adapters for frontend preview discovery.
//!
//! Pane output/exit and sandbox-provider events already arrive through the
//! host's waker-backed channels, so they are consumed directly by
//! [`crate::preview::PreviewSupervisor`]. The only filesystem work is this
//! one-shot scan, requested at startup and on worktree/config changes. There is
//! deliberately no interval timer, reconnect probe, or idle poll here.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use termwiz::terminal::TerminalWaker;
use thegn_core::preview::{MAX_PACKAGE_JSON_BYTES, PortHint, parse_package_scripts};
use tokio::sync::mpsc::UnboundedSender;

/// Result of one worktree/config discovery pass.
#[derive(Debug)]
pub(crate) struct ScanResult {
    pub generation: u64,
    pub worktree: String,
    pub configured: Vec<PortHint>,
    pub package: Vec<PortHint>,
    /// Bounded diagnostic text for an unreadable or invalid package manifest.
    pub diagnostic: Option<String>,
}

fn read_package(path: &Path) -> Result<Option<String>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    // Read one byte past the core parser's cap so oversized manifests are
    // rejected without ever allocating an unbounded file in the host.
    let mut text = String::new();
    file.take((MAX_PACKAGE_JSON_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Some(text))
}

/// Read `package.json` once off the render loop and publish the parsed result
/// with a waker pulse. Explicit configured ports ride the same generation so a
/// config reload atomically replaces both static discovery sources.
pub(crate) fn spawn_scan(
    generation: u64,
    worktree: PathBuf,
    configured_ports: Vec<u16>,
    tx: UnboundedSender<ScanResult>,
    waker: TerminalWaker,
) {
    let _ = std::thread::Builder::new()
        .name("preview-scan".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let configured = configured_ports
                .into_iter()
                .filter_map(PortHint::configured)
                .collect();
            let package_path = worktree.join("package.json");
            let (package, diagnostic) = match read_package(&package_path) {
                Ok(Some(text)) => match parse_package_scripts(&text) {
                    Ok(hints) => (hints, None),
                    Err(error) => (Vec::new(), Some(error)),
                },
                Ok(None) => (Vec::new(), None),
                Err(error) => (Vec::new(), Some(error)),
            };
            let result = ScanResult {
                generation,
                worktree: worktree.to_string_lossy().into_owned(),
                configured,
                package,
                diagnostic,
            };
            if tx.send(result).is_ok() {
                let _ = waker.wake(); // best-effort: the loop may already be shutting down
            }
        }); // best-effort: a failed one-shot scan leaves static targets unknown/unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_reader_is_missing_tolerant_and_bounded() {
        let root = std::env::temp_dir().join(format!("thegn-preview-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(read_package(&root.join("missing.json")).unwrap(), None);

        let path = root.join("package.json");
        std::fs::write(&path, "x".repeat(MAX_PACKAGE_JSON_BYTES + 100)).unwrap();
        assert_eq!(
            read_package(&path).unwrap().unwrap().len(),
            MAX_PACKAGE_JSON_BYTES + 1
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
