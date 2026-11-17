//! The credential-free rule for provider kinds where no host credential may
//! enter the sandbox ([`EnvProviderKind::credential_free`]): strip the
//! credential-bearing provisioning steps from a plan, and refuse them in the
//! applier (defence in depth).
//!
//! [`EnvProviderKind::credential_free`]: crate::config::EnvProviderKind::credential_free

/// Remove every credential-bearing step (agent configs, dotfiles, atuin sync,
/// home-closure push) from `plan`, returning the removed step labels. Spine
/// stub, filled by lane 5.
pub fn strip_credential_steps(_plan: &mut crate::envplan::EnvPlan) -> Vec<&'static str> {
    Vec::new()
}

/// Whether the applier refuses `kind` for a credential-free provider. Spine
/// stub, filled by lane 5.
pub fn refuses_step(_kind: &crate::envplan::StepKind) -> bool {
    false
}
