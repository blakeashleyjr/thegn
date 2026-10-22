//! Event-driven adapters for frontend preview discovery.
//!
//! Pane output/exit and sandbox-provider events already arrive through the
//! host's waker-backed channels, so they are consumed directly by
//! [`crate::preview::PreviewSupervisor`]. The only filesystem work is this
//! one-shot scan, requested at startup and on worktree/config changes. There is
//! deliberately no interval timer, reconnect probe, or idle poll here.

use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use termwiz::terminal::TerminalWaker;
use thegn_core::preview::{MAX_PACKAGE_JSON_BYTES, PortHint, parse_package_scripts};
use tokio::sync::mpsc::UnboundedSender;

/// Probe one batch of loopback candidates off the event loop. Each request gets
/// one bounded retry sequence, one result, and one waker pulse.
pub(crate) fn spawn_probe(
    requests: Vec<crate::preview::ProbeRequest>,
    tx: UnboundedSender<crate::preview::ProbeResult>,
    waker: TerminalWaker,
) {
    let spawn = std::thread::Builder::new().name("thegn-prev-prb".into()).spawn(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let backoff = [0, 500, 1_000, 2_000, 4_000, 8_000, 15_000, 30_000];
        let mut pending: Vec<(crate::preview::ProbeRequest, Vec<SocketAddr>)> = requests
            .into_iter()
            .map(|request| {
                let addresses = match request.host.as_str() {
                    "::1" => vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), request.port)],
                    "127.0.0.1" => vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), request.port)],
                    "localhost" => vec![
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), request.port),
                        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), request.port),
                    ],
                    _ => Vec::new(),
                };
                (request, addresses)
            })
            .collect();
        for delay in backoff {
            std::thread::sleep(Duration::from_millis(delay));
            let mut retry = Vec::with_capacity(pending.len());
            for (request, addresses) in pending {
                let ok = addresses.iter().any(|address| {
                    TcpStream::connect_timeout(address, Duration::from_millis(250)).is_ok()
                });
                if ok {
                    if tx.send(crate::preview::ProbeResult { request, ok }).is_err() {
                        tracing::debug!(target: "thegn::preview", "preview probe result receiver closed");
                    }
                    if let Err(error) = waker.wake() {
                        tracing::debug!(target: "thegn::preview", %error, "preview probe waker pulse failed");
                    }
                } else {
                    retry.push((request, addresses));
                }
            }
            pending = retry;
            if pending.is_empty() { break; }
        }
        for (request, _) in pending {
            if tx.send(crate::preview::ProbeResult { request, ok: false }).is_err() {
                tracing::debug!(target: "thegn::preview", "preview probe result receiver closed");
            }
            if let Err(error) = waker.wake() {
                tracing::debug!(target: "thegn::preview", %error, "preview probe waker pulse failed");
            }
        }
    });
    if let Err(error) = spawn {
        tracing::debug!(target: "thegn::preview", %error, "preview probe thread spawn failed");
    }
}

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
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if !metadata.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
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
        .name("thegn-prev-scan".into())
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

    #[test]
    fn package_reader_rejects_non_regular_targets() {
        let root = std::env::temp_dir().join(format!(
            "thegn-preview-watch-special-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("package.json");
        std::fs::create_dir(&path).unwrap();
        assert!(
            read_package(&path)
                .unwrap_err()
                .contains("not a regular file")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
