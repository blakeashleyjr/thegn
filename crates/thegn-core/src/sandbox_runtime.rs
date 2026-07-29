//! OCI runtime (`[sandbox] oci_runtime`) host requirements + the pure
//! keep-or-degrade decision. Split out of the (ratchet-capped) `sandbox.rs`.
//!
//! A non-default runtime (`runsc`, `krun`) only works if its pieces are present:
//! the runtime binary on `PATH`, and — for libkrun's microVM — `/dev/kvm`. When
//! they're missing, `podman/docker create --runtime <x>` would fail; rather than
//! hard-fail the pane we degrade to the daemon default and surface a note. This
//! module owns only the *decision*; the host does the fs/`PATH` probe and applies
//! it (so the subprocess/fs seam stays out of the coverage-gated core).

/// What a non-default OCI runtime needs on the host to actually run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeReq {
    /// The runtime binary podman/docker invokes (looked up on `PATH`).
    pub binary: &'static str,
    /// libkrun boots a hardware-virtualized microVM, so it needs `/dev/kvm`.
    /// gVisor's `runsc` does not (its default platform is `ptrace`).
    pub needs_kvm: bool,
}

/// The host requirements for a runtime, or `None` for the daemon defaults
/// (`runc`/`crun`, or empty) — assumed present, nothing to probe.
pub fn runtime_req(runtime: &str) -> Option<RuntimeReq> {
    match runtime.trim() {
        "runsc" => Some(RuntimeReq {
            binary: "runsc",
            needs_kvm: false,
        }),
        "krun" => Some(RuntimeReq {
            binary: "krun",
            needs_kvm: true,
        }),
        _ => None,
    }
}

/// The pure decision: keep the requested runtime, or drop back to the daemon
/// default with a human-readable reason. `binary_present`/`kvm_present` are only
/// consulted when the runtime declares a requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeDecision {
    /// Use the requested runtime as-is.
    Keep,
    /// Fall back to the daemon default; the string is the user-facing reason.
    Degrade(String),
}

/// Decide whether `runtime` can run given probed host facts. Unset/blank or a
/// default runtime (`runc`/`crun`/unknown) always [`Keep`](RuntimeDecision::Keep)
/// — we don't second-guess the daemon's own default.
pub fn decide(runtime: Option<&str>, binary_present: bool, kvm_present: bool) -> RuntimeDecision {
    let Some(rt) = runtime.map(str::trim).filter(|r| !r.is_empty()) else {
        return RuntimeDecision::Keep;
    };
    let Some(req) = runtime_req(rt) else {
        return RuntimeDecision::Keep;
    };
    if !binary_present {
        return RuntimeDecision::Degrade(format!(
            "oci_runtime {rt:?}: runtime binary {:?} not found on PATH; using the daemon default",
            req.binary
        ));
    }
    if req.needs_kvm && !kvm_present {
        return RuntimeDecision::Degrade(format!(
            "oci_runtime {rt:?}: /dev/kvm not available; using the daemon default"
        ));
    }
    RuntimeDecision::Keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_table() {
        assert_eq!(
            runtime_req("runsc"),
            Some(RuntimeReq {
                binary: "runsc",
                needs_kvm: false
            })
        );
        assert_eq!(
            runtime_req("krun"),
            Some(RuntimeReq {
                binary: "krun",
                needs_kvm: true
            })
        );
        // Defaults / unknowns declare no requirement.
        assert_eq!(runtime_req("runc"), None);
        assert_eq!(runtime_req("crun"), None);
        assert_eq!(runtime_req(""), None);
        assert_eq!(runtime_req("bogus"), None);
    }

    #[test]
    fn unset_or_default_always_keeps() {
        assert_eq!(decide(None, false, false), RuntimeDecision::Keep);
        assert_eq!(decide(Some(""), false, false), RuntimeDecision::Keep);
        assert_eq!(decide(Some("  "), false, false), RuntimeDecision::Keep);
        assert_eq!(decide(Some("crun"), false, false), RuntimeDecision::Keep);
    }

    #[test]
    fn runsc_needs_only_its_binary() {
        assert_eq!(decide(Some("runsc"), true, false), RuntimeDecision::Keep);
        assert!(matches!(
            decide(Some("runsc"), false, true),
            RuntimeDecision::Degrade(_)
        ));
    }

    #[test]
    fn krun_needs_binary_and_kvm() {
        assert_eq!(decide(Some("krun"), true, true), RuntimeDecision::Keep);
        // Missing binary is reported even if kvm is present.
        match decide(Some("krun"), false, true) {
            RuntimeDecision::Degrade(m) => assert!(m.contains("PATH"), "{m}"),
            d => panic!("expected degrade, got {d:?}"),
        }
        // Binary present but no kvm ⇒ degrade on kvm.
        match decide(Some("krun"), true, false) {
            RuntimeDecision::Degrade(m) => assert!(m.contains("/dev/kvm"), "{m}"),
            d => panic!("expected degrade, got {d:?}"),
        }
    }
}
