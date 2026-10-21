//! Debugger integration — pure domain logic for launching a debugger session.
//!
//! thegn's debugger is BugStalker (`bs`): a self-contained Rust/Linux TUI
//! debugger installed from crates.io. It is acquired and pinned through the
//! shared [`crate::managed_tool`] resolver (a `Cargo` source), and launched as an
//! ordinary interactive program. Because a debug session is started inside a
//! thegn pane, it inherits that pane's sandbox and remote placement for free —
//! no DAP client and no extra wrapping needed.
//!
//! Everything here is pure and unit-tested: the pinned tool spec, the
//! platform gate (BugStalker is Linux-x86-64 only), and the session-argv
//! builders. The `cargo install` fetch and the process exec live in the host.

use crate::managed_tool::{Arch, ManagedTool, Os, Resolution, ToolOverride, UpdatePolicy};
use crate::seam::{Availability, ErrorClass, Kind, Probe, ProbeReport, SeamError};

/// Pinned BugStalker version (crates.io `bugstalker`). Bump to adopt a newer
/// debugger release.
pub const BS_PIN: &str = "0.4.6";

/// The BugStalker managed-tool spec: `cargo install bugstalker` (binary `bs`),
/// resolved override → PATH (`bs`) → managed `~/.thegn/tools/bugstalker/bin/bs`.
pub fn bs_tool() -> ManagedTool {
    ManagedTool::cargo("bugstalker", "bugstalker", "bs", BS_PIN)
        .with_policy(UpdatePolicy::Once)
        .with_path_fallbacks(&["bs"])
}

/// Whether BugStalker can run on `(os, arch)`. BugStalker supports **Linux
/// x86-64 only**; everywhere else thegn refuses rather than attempting an
/// install/launch that cannot work.
pub fn bs_supported(os: Os, arch: Arch) -> bool {
    os == Os::Linux && arch == Arch::X64
}

/// [`bs_supported`] for the host thegn is running on. `false` when the
/// platform can't be determined.
pub fn platform_supported() -> bool {
    match (Os::current(), Arch::current()) {
        (Some(os), Some(arch)) => bs_supported(os, arch),
        _ => false,
    }
}

/// A short, user-facing reason a debug session can't start here (or `None` when
/// it can). Used by the CLI/doctor to explain the platform gate.
pub fn unsupported_reason() -> Option<&'static str> {
    if platform_supported() {
        None
    } else {
        Some(
            "BugStalker supports only Linux on x86-64; install it there (or via your distro / nix) to debug on this host",
        )
    }
}

/// argv to launch `program` (with `args`) under the debugger: `bs <program>
/// [args…]` — the debugee is a positional argument and the rest are passed
/// through to it.
pub fn launch_argv(bin: &str, program: &str, args: &[String]) -> Vec<String> {
    let mut argv = vec![bin.to_string(), program.to_string()];
    argv.extend(args.iter().cloned());
    argv
}

/// argv to attach the debugger to a running process: `bs -p <pid>`.
pub fn attach_argv(bin: &str, pid: i64) -> Vec<String> {
    vec![bin.to_string(), "-p".to_string(), pid.to_string()]
}

/// Debugger implementations accepted by the provider-neutral debug seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugAdapterKind {
    BugStalker,
    Dap,
}

impl Kind for DebugAdapterKind {
    const ALL: &'static [Self] = &[Self::BugStalker, Self::Dap];

    fn as_str(self) -> &'static str {
        match self {
            Self::BugStalker => "bugstalker",
            Self::Dap => "dap",
        }
    }

    fn is_reserved(self) -> bool {
        matches!(self, Self::Dap)
    }
}

/// Operations supported by a debugger adapter. Optional operations are
/// available only when their corresponding capability is true.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct DebugAdapterCaps {
    pub launch: bool,
    pub attach: bool,
    pub breakpoints: bool,
    pub stepping: bool,
    pub variables: bool,
}

/// A stable refusal from the debug adapter seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugAdapterError {
    Unsupported(&'static str),
    Platform(&'static str),
}

impl std::fmt::Display for DebugAdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(op) => write!(f, "debug adapter does not support {op}"),
            Self::Platform(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for DebugAdapterError {}

impl SeamError for DebugAdapterError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Unsupported(_) => ErrorClass::Unsupported,
            Self::Platform(_) => ErrorClass::NotInstalled,
        }
    }

    fn unsupported(op: &'static str) -> Self {
        Self::Unsupported(op)
    }
}

/// Provider-neutral debugger launch planning seam.
pub trait DebugAdapter: Probe + Send + Sync {
    fn kind(&self) -> DebugAdapterKind;
    fn caps(&self) -> DebugAdapterCaps;
    fn launch_argv(
        &self,
        bin: &str,
        program: &str,
        args: &[String],
    ) -> Result<Vec<String>, DebugAdapterError>;
    fn attach_argv(&self, _bin: &str, _pid: i64) -> Result<Vec<String>, DebugAdapterError> {
        Err(DebugAdapterError::unsupported("attach_argv"))
    }
}

/// BugStalker adapter; its default binary follows managed-tool resolution.
pub struct BugStalkerAdapter {
    over: Option<ToolOverride>,
}

impl BugStalkerAdapter {
    fn new(over: Option<ToolOverride>) -> Self {
        Self { over }
    }
}

impl DebugAdapter for BugStalkerAdapter {
    fn kind(&self) -> DebugAdapterKind {
        DebugAdapterKind::BugStalker
    }

    fn caps(&self) -> DebugAdapterCaps {
        DebugAdapterCaps {
            launch: true,
            attach: true,
            ..DebugAdapterCaps::default()
        }
    }

    fn launch_argv(
        &self,
        bin: &str,
        program: &str,
        args: &[String],
    ) -> Result<Vec<String>, DebugAdapterError> {
        if let Some(reason) = unsupported_reason() {
            return Err(DebugAdapterError::Platform(reason));
        }
        Ok(launch_argv(bin, program, args))
    }

    fn attach_argv(&self, bin: &str, pid: i64) -> Result<Vec<String>, DebugAdapterError> {
        if let Some(reason) = unsupported_reason() {
            return Err(DebugAdapterError::Platform(reason));
        }
        Ok(attach_argv(bin, pid))
    }
}

impl Probe for BugStalkerAdapter {
    fn probe(&self) -> ProbeReport {
        let unsupported = unsupported_reason();
        let resolution = if unsupported.is_none() {
            Some(bs_tool().resolve(self.over.as_ref(), crate::util::which_path))
        } else {
            None
        };
        bugstalker_report(resolution.as_ref(), unsupported)
    }
}

fn bugstalker_report(
    resolution: Option<&Resolution>,
    unsupported: Option<&'static str>,
) -> ProbeReport {
    let availability = if let Some(reason) = unsupported {
        Availability::Unavailable(reason.to_string())
    } else if matches!(resolution, Some(Resolution::Managed { current: false, .. })) {
        Availability::Degraded(format!(
            "managed BugStalker {BS_PIN} not installed; run `thegn debug setup`"
        ))
    } else {
        Availability::Ready
    };
    let mut report =
        ProbeReport::new("debug", "bugstalker", availability).with_caps(&DebugAdapterCaps {
            launch: true,
            attach: true,
            ..DebugAdapterCaps::default()
        });
    if let Some(resolution) = resolution {
        report = report.note(format!("bs: {} ({})", resolution.path(), resolution.tier()));
    }
    report.note("dap: reserved (THE-102)")
}

/// Build the selected adapter. BugStalker is currently the only selectable
/// implementation; DAP remains reserved vocabulary.
pub fn adapter_for(cfg: &crate::config::Config) -> Box<dyn DebugAdapter> {
    let tool = bs_tool();
    Box::new(BugStalkerAdapter::new(
        cfg.managed_tools.get(&tool.name).cloned(),
    ))
}

/// Probe the selected debugger adapter without reporting reserved DAP as a
/// configured provider.
pub fn probes(cfg: &crate::config::Config) -> Vec<ProbeReport> {
    vec![adapter_for(cfg).probe()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bs_tool_is_cargo_sourced_and_pinned() {
        let t = bs_tool();
        assert_eq!(t.name, "bugstalker");
        assert_eq!(t.version, BS_PIN);
        // Cargo source ⇒ no release asset; resolves to a managed bin named `bs`.
        assert_eq!(t.asset_for(Os::Linux, Arch::X64), None);
        assert!(t.bin_path().ends_with("bin/bs"));
        assert!(t.path_fallbacks.contains(&"bs".to_string()));
    }

    #[test]
    fn supported_only_on_linux_x64() {
        assert!(bs_supported(Os::Linux, Arch::X64));
        assert!(!bs_supported(Os::Linux, Arch::Arm64));
        assert!(!bs_supported(Os::Macos, Arch::X64));
        assert!(!bs_supported(Os::Macos, Arch::Arm64));
        assert!(!bs_supported(Os::Windows, Arch::X64));
        // The reason string is present iff the host is unsupported.
        assert_eq!(unsupported_reason().is_none(), platform_supported());
    }

    #[test]
    fn launch_argv_builds_program_and_args() {
        assert_eq!(
            launch_argv("bs", "./target/debug/app", &["--flag".into(), "x".into()]),
            vec![
                "bs".to_string(),
                "./target/debug/app".into(),
                "--flag".into(),
                "x".into()
            ]
        );
        // No extra args → just the debugee.
        assert_eq!(
            launch_argv("/opt/bs", "prog", &[]),
            vec!["/opt/bs".to_string(), "prog".into()]
        );
    }

    #[test]
    fn attach_argv_builds_pid_form() {
        assert_eq!(
            attach_argv("bs", 4242),
            vec!["bs".to_string(), "-p".into(), "4242".into()]
        );
    }

    #[test]
    fn kind_has_bugstalker_implemented_and_dap_reserved() {
        assert_eq!(
            DebugAdapterKind::implemented().collect::<Vec<_>>(),
            vec![DebugAdapterKind::BugStalker]
        );
        assert_eq!(DebugAdapterKind::BugStalker.as_str(), "bugstalker");
        assert_eq!(DebugAdapterKind::Dap.as_str(), "dap");
        assert!(DebugAdapterKind::Dap.is_reserved());
    }

    #[test]
    fn bugstalker_adapter_argv_matches_the_legacy_builders() {
        use crate::seam::SeamError;

        let adapter = BugStalkerAdapter::new(None);
        let args = vec!["--flag".to_string(), "x".to_string()];
        assert_eq!(
            DebugAdapterError::Platform("unsupported").class(),
            ErrorClass::NotInstalled
        );
        assert_eq!(
            DebugAdapterError::Platform("unsupported").to_string(),
            "unsupported"
        );
        if platform_supported() {
            assert_eq!(
                adapter.launch_argv("bs", "prog", &args).unwrap(),
                launch_argv("bs", "prog", &args)
            );
            assert_eq!(
                adapter.attach_argv("bs", 4242).unwrap(),
                attach_argv("bs", 4242)
            );
        } else {
            assert!(matches!(
                adapter.launch_argv("bs", "prog", &args),
                Err(DebugAdapterError::Platform(_))
            ));
            assert!(matches!(
                adapter.attach_argv("bs", 4242),
                Err(DebugAdapterError::Platform(_))
            ));
        }
    }

    #[test]
    fn bugstalker_report_states() {
        let unsupported_reason = "unsupported platform";
        let unavailable = bugstalker_report(None, Some(unsupported_reason));
        assert_eq!(
            unavailable.availability,
            Availability::Unavailable(unsupported_reason.into())
        );

        let managed = Resolution::Managed {
            path: "/managed/bs".into(),
            current: false,
        };
        let degraded = bugstalker_report(Some(&managed), None);
        assert!(matches!(
            &degraded.availability,
            Availability::Degraded(reason) if reason.contains("thegn debug setup")
        ));

        let on_path = Resolution::OnPath {
            path: "/usr/bin/bs".into(),
        };
        let ready = bugstalker_report(Some(&on_path), None);
        assert_eq!(ready.availability, Availability::Ready);
        assert!(
            ready
                .notes
                .iter()
                .any(|note| note == "bs: /usr/bin/bs (path)")
        );
        assert!(
            ready
                .notes
                .iter()
                .any(|note| note == "dap: reserved (THE-102)")
        );
        assert_eq!(ready.caps["launch"], serde_json::json!(true));
        assert_eq!(ready.caps["attach"], serde_json::json!(true));
        assert_eq!(ready.caps["breakpoints"], serde_json::json!(false));
        assert_eq!(ready.caps["stepping"], serde_json::json!(false));
        assert_eq!(ready.caps["variables"], serde_json::json!(false));
    }

    #[test]
    fn default_attach_is_unsupported() {
        struct LaunchOnly;

        impl Probe for LaunchOnly {
            fn probe(&self) -> ProbeReport {
                ProbeReport::new("debug", "fixture", Availability::Ready)
            }
        }

        impl DebugAdapter for LaunchOnly {
            fn kind(&self) -> DebugAdapterKind {
                DebugAdapterKind::BugStalker
            }

            fn caps(&self) -> DebugAdapterCaps {
                DebugAdapterCaps {
                    launch: true,
                    ..DebugAdapterCaps::default()
                }
            }

            fn launch_argv(
                &self,
                _bin: &str,
                _program: &str,
                _args: &[String],
            ) -> Result<Vec<String>, DebugAdapterError> {
                Ok(Vec::new())
            }
        }

        use crate::seam::SeamError;
        let error = LaunchOnly.attach_argv("bs", 1).unwrap_err();
        assert_eq!(error.class(), ErrorClass::Unsupported);
        assert_eq!(
            error.to_string(),
            "debug adapter does not support attach_argv"
        );
    }

    #[test]
    fn adapter_factory_and_probe_use_configured_override() {
        let mut cfg = crate::config::Config::default();
        cfg.managed_tools.insert(
            bs_tool().name,
            ToolOverride {
                path: "/fixture/bs".into(),
                args: Vec::new(),
            },
        );
        let adapter = adapter_for(&cfg);
        assert_eq!(adapter.kind(), DebugAdapterKind::BugStalker);
        assert_eq!(
            adapter.caps(),
            DebugAdapterCaps {
                launch: true,
                attach: true,
                ..DebugAdapterCaps::default()
            }
        );

        let reports = probes(&cfg);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].seam, "debug");
        assert_eq!(reports[0].id, "bugstalker");
        if platform_supported() {
            assert!(
                reports[0]
                    .notes
                    .iter()
                    .any(|note| note == "bs: /fixture/bs (override)")
            );
        } else {
            assert!(matches!(
                &reports[0].availability,
                Availability::Unavailable(reason) if reason.contains("Linux on x86-64")
            ));
            assert!(!reports[0].notes.iter().any(|note| note.starts_with("bs:")));
        }
    }
}
