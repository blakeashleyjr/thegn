//! Rendering of sandbox-prepare failures for user-facing surfaces (THE-306 bullet 6).

/// The error rendered for a user: led by the typed refusal class and remediation when the chain
/// carries a `thegn_core::sandbox::EnterError`, otherwise exactly `error.to_string()` (the
/// outermost message, as before), so untyped errors keep their historic text.
pub(crate) fn failure_detail(error: &anyhow::Error) -> String {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<thegn_core::sandbox::EnterError>()
            .is_some()
    }) {
        thegn_core::sandbox::refusal_context(error)
    } else {
        error.to_string()
    }
}

/// `sandbox <backend> failed: <detail>`: the line `prepare_sandbox_env` records for a failed candidate.
pub(crate) fn sandbox_failed_message(backend_label: &str, error: &anyhow::Error) -> String {
    format!("sandbox {backend_label} failed: {}", failure_detail(error))
}

#[cfg(test)]
mod tests {
    use super::{failure_detail, sandbox_failed_message};
    use thegn_core::sandbox::{EnterError, Refusal, RefusalClass, TUNNEL_ONLY_REFUSAL};

    fn tunnel_refusal() -> anyhow::Error {
        anyhow::Error::new(EnterError::Refused(Refusal {
            class: RefusalClass::TunnelUnenforced,
            backend: "bwrap".into(),
            path: None,
            message: TUNNEL_ONLY_REFUSAL.into(),
        }))
    }

    #[test]
    fn failure_detail_leads_with_the_class_and_remediation() {
        assert_eq!(
            failure_detail(&tunnel_refusal()),
            format!(
                "sandbox entry refused [tunnel-unenforced]: use profile 'standard' with [sandbox.vpn], or 'sealed' for no network: {}",
                TUNNEL_ONLY_REFUSAL
            )
        );
    }

    #[test]
    fn failure_detail_finds_a_refusal_below_context() {
        let error = tunnel_refusal().context("outer step");
        let detail = failure_detail(&error);
        assert!(detail.starts_with("sandbox entry refused [tunnel-unenforced]"));
        assert!(detail.contains("outer step"));
    }

    #[test]
    fn failure_detail_keeps_untyped_text_exact() {
        assert_eq!(failure_detail(&anyhow::anyhow!("boom")), "boom");
        assert_eq!(
            failure_detail(&anyhow::anyhow!("inner").context("outer")),
            "outer"
        );
    }

    #[test]
    fn sandbox_failed_message_prefixes_backend_and_class() {
        assert!(
            sandbox_failed_message("bwrap", &tunnel_refusal())
                .starts_with("sandbox bwrap failed: sandbox entry refused [tunnel-unenforced]")
        );
    }

    #[test]
    fn sandbox_failed_message_keeps_untyped_text_exact() {
        assert_eq!(
            sandbox_failed_message("docker", &anyhow::anyhow!("boom")),
            "sandbox docker failed: boom"
        );
    }
}
