//! Inbound host discovery — the I/O seam over the local mesh-VPN client.
//!
//! The pure model + parser live in `thegn_core::tailnet`; this module is the
//! subprocess boundary (running the user's `tailscale` client) and the
//! `thegn doctor` probe. It mirrors the division of labor in every other svc
//! seam: pure builders/parsers are unit-tested in core, the subprocess is the
//! I/O seam exercised by `test/smoke.sh`.
//!
//! Only one kind is implemented — `tailnet` (control-plane agnostic, so it also
//! serves headscale). `mdns`/`consul` are `reserved` (see
//! `thegn_core::config_host_discovery`): the factory returns `None` for them,
//! and `kind_coverage` pins that against the config enum.
//!
//! Discovery runs only on explicit user action and off the event loop; there is
//! deliberately no polling path in this module.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use thegn_core::config::{HostDiscoveryConfig, HostDiscoveryKind, TailnetDiscoveryConfig};
use thegn_core::seam::{Availability, BoxFuture, ErrorClass, ProbeReport, SeamError};
use thegn_core::tailnet::{self, BackendState, HostCandidate, TailnetStatus};

use crate::seam::JoinFailure;

/// The seam name every probe/report carries.
pub const SEAM: &str = "host_discovery";

/// Hard cap on any single `tailscale` invocation. The client talks to the local
/// tailscaled over a unix socket, so this only bites when the daemon is wedged —
/// which must never hang discovery or `doctor`.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(6);

/// The seam's error type. Carries the uniform [`ErrorClass`] every seam exposes:
/// `NotInstalled` (no client binary), `NotConfigured` (logged out), `Transient`
/// (tailscaled unreachable), `Auth` (an ACL/sshd refusal at connect time — set
/// by the connect path, not discovery), `Other`.
#[derive(Debug, Clone)]
pub struct DiscoveryError {
    class: ErrorClass,
    msg: String,
}

impl DiscoveryError {
    fn new(class: ErrorClass, msg: impl Into<String>) -> Self {
        DiscoveryError {
            class,
            msg: msg.into(),
        }
    }
    pub fn class(&self) -> ErrorClass {
        self.class
    }
    pub fn message(&self) -> &str {
        &self.msg
    }
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for DiscoveryError {}

impl SeamError for DiscoveryError {
    fn class(&self) -> ErrorClass {
        self.class
    }
    fn unsupported(op: &'static str) -> Self {
        DiscoveryError::new(
            ErrorClass::Unsupported,
            format!("host discovery does not support `{op}`"),
        )
    }
}

impl From<JoinFailure> for DiscoveryError {
    fn from(j: JoinFailure) -> Self {
        DiscoveryError::new(ErrorClass::Other, j.to_string())
    }
}

/// The inbound host-discovery seam. Object-safe: async methods return
/// [`BoxFuture`], so a caller holds a `Box<dyn HostDiscovery>`.
pub trait HostDiscovery: Send + Sync {
    /// Stable id (`"tailnet"`).
    fn kind(&self) -> &'static str;
    /// Enumerate remote-host candidates. Runs off the runtime's worker threads
    /// (blocking subprocess on the blocking pool). Logged-out ⇒ `NotConfigured`.
    fn discover(&self) -> BoxFuture<'_, Result<Vec<HostCandidate>, DiscoveryError>>;
    /// The `thegn doctor` self-description (cheap, local — no network).
    fn probe(&self) -> ProbeReport;
}

/// Build the provider a loaded config selects, or `None` for a reserved kind.
pub fn build(cfg: &HostDiscoveryConfig) -> Option<Box<dyn HostDiscovery>> {
    match cfg.kind {
        HostDiscoveryKind::Tailnet => Some(Box::new(TailnetDiscovery::new(cfg.tailnet.clone()))),
        HostDiscoveryKind::Mdns | HostDiscoveryKind::Consul => None,
    }
}

/// [`build`] keyed by kind alone (default config) — the seam's `kind_coverage`
/// factory: `Some` exactly for the implemented kind.
pub fn for_kind(kind: HostDiscoveryKind) -> Option<Box<dyn HostDiscovery>> {
    match kind {
        HostDiscoveryKind::Tailnet => Some(Box::new(TailnetDiscovery::new(
            TailnetDiscoveryConfig::default(),
        ))),
        HostDiscoveryKind::Mdns | HostDiscoveryKind::Consul => None,
    }
}

/// The `tailnet` kind: enumerate peers from the local tailscale client.
pub struct TailnetDiscovery {
    cfg: TailnetDiscoveryConfig,
}

impl TailnetDiscovery {
    pub fn new(cfg: TailnetDiscoveryConfig) -> Self {
        TailnetDiscovery { cfg }
    }

    fn bin(&self) -> String {
        let b = self.cfg.tailscale_bin.trim();
        if b.is_empty() {
            "tailscale".to_string()
        } else {
            b.to_string()
        }
    }
}

impl HostDiscovery for TailnetDiscovery {
    fn kind(&self) -> &'static str {
        "tailnet"
    }

    fn discover(&self) -> BoxFuture<'_, Result<Vec<HostCandidate>, DiscoveryError>> {
        let bin = self.bin();
        let online_only = self.cfg.online_only;
        let tag = self.cfg.tag_filter.clone();
        // On the blocking pool: a subprocess never stalls the runtime workers.
        crate::seam::blocking(move || {
            let status = fetch_status(&bin)?;
            if !status.backend_state.logged_in() {
                return Err(DiscoveryError::new(
                    ErrorClass::NotConfigured,
                    logged_out_message(status.backend_state),
                ));
            }
            Ok(tailnet::filter_candidates(&status.peers, online_only, &tag))
        })
    }

    fn probe(&self) -> ProbeReport {
        probe_tailnet(&self.bin())
    }
}

/// Is the client binary resolvable? (An absolute/relative path must exist; a
/// bare name must be on `PATH`.) Checked before every exec so a missing client
/// classifies as `NotInstalled` without spawning anything.
fn binary_present(bin: &str) -> bool {
    if bin.contains('/') {
        std::path::Path::new(bin).exists()
    } else {
        thegn_core::util::which_path(bin).is_some()
    }
}

fn logged_out_message(state: BackendState) -> String {
    match state {
        BackendState::NeedsLogin => {
            "tailscale is logged out — run `tailscale up` (no candidates)".to_string()
        }
        BackendState::Stopped => {
            "tailscale is stopped — run `tailscale up` (no candidates)".to_string()
        }
        other => format!(
            "tailscale is not logged in (backend state: {})",
            other.as_str()
        ),
    }
}

/// Run `tailscale status --json` and parse it. Classifies the ways it can fail:
/// missing binary ⇒ `NotInstalled`; tailscaled unreachable (non-zero exit with
/// no parseable JSON) ⇒ `Transient`; anything else ⇒ `Other`.
fn fetch_status(bin: &str) -> Result<TailnetStatus, DiscoveryError> {
    if !binary_present(bin) {
        return Err(DiscoveryError::new(
            ErrorClass::NotInstalled,
            format!(
                "`{bin}` not found on PATH — install Tailscale (https://tailscale.com/download) \
                 or set [host_discovery.tailnet] tailscale_bin"
            ),
        ));
    }
    let out = run_timed(&[bin, "status", "--json"])?;
    // `tailscale status --json` prints JSON to stdout even when logged out
    // (exit 0). When tailscaled is unreachable it exits non-zero with a stderr
    // message and no JSON — the parse fails and we classify that as transient.
    match tailnet::parse_status_json(&out.stdout) {
        Ok(status) => Ok(status),
        Err(_) if !out.ok => Err(DiscoveryError::new(
            ErrorClass::Transient,
            transient_message(&out.stderr),
        )),
        Err(e) => Err(DiscoveryError::new(ErrorClass::Other, e.to_string())),
    }
}

fn transient_message(stderr: &str) -> String {
    let detail = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if detail.is_empty() {
        "tailscaled is unreachable (is the Tailscale service running?)".to_string()
    } else {
        format!("tailscaled is unreachable: {}", detail.trim())
    }
}

/// The doctor probe for the tailnet kind. Cheap + local: a `which`, then two
/// bounded local calls (`status`, `debug prefs`) only if the client is present.
fn probe_tailnet(bin: &str) -> ProbeReport {
    if !binary_present(bin) {
        return ProbeReport::new(
            SEAM,
            "tailnet",
            Availability::Unavailable(format!(
                "`{bin}` not found on PATH — install Tailscale to enable host discovery"
            )),
        );
    }
    match fetch_status(bin) {
        Ok(status) => {
            // The authoritative control URL (surfaces headscale) — best-effort.
            let control_url = fetch_control_url(bin);
            let (avail, notes) = tailnet::probe_summary(&status, control_url.as_deref());
            let mut report = ProbeReport::new(SEAM, "tailnet", avail);
            for note in notes {
                report = report.note(note);
            }
            report
        }
        Err(e) => ProbeReport::new(SEAM, "tailnet", Availability::Unavailable(e.msg)),
    }
}

/// Best-effort control URL from `tailscale debug prefs` (JSON). `None` on any
/// failure — the probe reports "unknown" honestly.
fn fetch_control_url(bin: &str) -> Option<String> {
    let out = run_timed(&[bin, "debug", "prefs"]).ok()?;
    if !out.ok {
        return None;
    }
    tailnet::parse_prefs_control_url(&out.stdout)
}

/// The captured result of a bounded subprocess run.
struct Captured {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run `argv` to completion or [`CLIENT_TIMEOUT`], killing a wedged child. A
/// spawn failure or timeout is a `Transient` [`DiscoveryError`] (the local
/// daemon might come back); the child's own non-zero exit is reported via
/// `ok=false` for the caller to classify.
///
/// Both pipes are drained on their own threads so a large `status --json`
/// (a big tailnet easily exceeds the OS pipe buffer) never deadlocks the child
/// on a full pipe while we wait on it.
fn run_timed(argv: &[&str]) -> Result<Captured, DiscoveryError> {
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            DiscoveryError::new(
                ErrorClass::Transient,
                format!("could not run `{}`: {e}", argv.join(" ")),
            )
        })?;

    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = pipe {
                // best-effort: whatever was read is still returned, and a
                // partial read cannot pass as success — the caller parses this
                // as JSON, so a truncated payload fails there with a real error.
                let _ = p.read_to_string(&mut s); // best-effort: partial read fails the JSON parse below with a real error
            }
            s
        })
    };
    let out_handle = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err_handle = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + CLIENT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // best-effort: reap the wedged child (unblocks the readers).
                    let _ = child.kill(); // best-effort: kill the wedged child (see above)
                    let _ = child.wait(); // best-effort: reap-or-not is terminal here
                    let _ = out_handle.join(); // best-effort: join failure must not mask the timeout
                    let _ = err_handle.join(); // best-effort: join failure must not mask the timeout
                    return Err(DiscoveryError::new(
                        ErrorClass::Transient,
                        format!(
                            "`{}` timed out after {}s (tailscaled unresponsive)",
                            argv.join(" "),
                            CLIENT_TIMEOUT.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = out_handle.join(); // best-effort: join failure must not mask the error below
                let _ = err_handle.join(); // best-effort: join failure must not mask the error below
                return Err(DiscoveryError::new(
                    ErrorClass::Other,
                    format!("waiting on `{}`: {e}", argv.join(" ")),
                ));
            }
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(Captured {
        ok: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_coverage_pins_implemented_or_reserved() {
        crate::seam::kind_coverage(for_kind);
    }

    #[test]
    fn build_selects_tailnet_and_refuses_reserved() {
        let mut cfg = HostDiscoveryConfig::default();
        assert!(build(&cfg).is_some());
        assert_eq!(build(&cfg).unwrap().kind(), "tailnet");
        cfg.kind = HostDiscoveryKind::Mdns;
        assert!(build(&cfg).is_none(), "reserved kind ⇒ no provider");
    }

    #[test]
    fn missing_binary_is_not_installed_not_a_panic() {
        // An absolute path that cannot exist ⇒ NotInstalled, named, no spawn.
        let err = fetch_status("/nonexistent/definitely-not-tailscale-xyz").unwrap_err();
        assert_eq!(err.class(), ErrorClass::NotInstalled);
        assert!(err.message().contains("definitely-not-tailscale-xyz"));
        assert!(err.message().contains("install Tailscale"));
    }

    #[test]
    fn probe_of_missing_binary_is_unavailable() {
        let d = TailnetDiscovery::new(TailnetDiscoveryConfig {
            tailscale_bin: "/nonexistent/tailscale-xyz".into(),
            ..TailnetDiscoveryConfig::default()
        });
        let report = d.probe();
        assert_eq!(report.seam, SEAM);
        assert_eq!(report.id, "tailnet");
        match report.availability {
            Availability::Unavailable(why) => assert!(why.contains("tailscale-xyz")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn binary_present_checks_path_shape() {
        assert!(!binary_present("/nonexistent/xyz-tailscale"));
        assert!(!binary_present("thegn-definitely-not-a-binary-xyz"));
        // A ubiquitous binary IS present (bare-name PATH lookup works).
        assert!(binary_present("sh") || binary_present("cmd"));
    }

    #[test]
    fn error_classes_and_seam_error_contract() {
        assert_eq!(
            DiscoveryError::unsupported("promote").class(),
            ErrorClass::Unsupported
        );
        let transient = DiscoveryError::new(ErrorClass::Transient, "x");
        assert!(transient.is_transient());
        assert!(!transient.falls_through(), "transient is a final answer");
        assert!(DiscoveryError::new(ErrorClass::NotInstalled, "x").falls_through());
        // JoinFailure maps to Other.
        let joined = DiscoveryError::from(JoinFailure("boom".into()));
        assert_eq!(joined.class(), ErrorClass::Other);
    }

    #[test]
    fn logged_out_messages_name_the_remedy() {
        assert!(logged_out_message(BackendState::NeedsLogin).contains("tailscale up"));
        assert!(logged_out_message(BackendState::Stopped).contains("tailscale up"));
        assert!(logged_out_message(BackendState::NoState).contains("not logged in"));
    }

    #[test]
    fn transient_message_summarizes_stderr() {
        assert!(transient_message("").contains("unreachable"));
        let m = transient_message("failed to connect to local tailscaled\nextra");
        assert!(m.contains("failed to connect to local tailscaled"));
        assert!(!m.contains("extra"), "only the first line is summarized");
    }
}
