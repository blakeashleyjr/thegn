//! Value-free CI credential placement reporting.
//!
//! The report describes the credential path, not the credential itself.  Keep
//! this seam pure so doctor and provider probes can share the same wording and
//! so a configured token, reference, or secret path can never enter output.

use crate::secret::SecretResolution;

/// The CI provider whose credential path is being described.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Github,
    Gitlab,
}

/// The placement family used by the CI command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Local,
    Ssh,
    Provider,
}

/// Credential state with no secret-bearing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMode {
    /// GitHub's `gh` login, which reads host env/keyring locally or its own
    /// login on a remote placement.
    GithubCliManaged,
    /// No GitLab token is available; `glab` must use its own login.
    GitlabTokenRefAbsent,
    /// A GitLab token resolved. `explicit` distinguishes the shipped ambient
    /// env default, which permits remote fallback, from an explicit token,
    /// which the GitLab command refuses to forward remotely.
    GitlabTokenRefPresent { explicit: bool },
    /// A configured GitLab token reference did not resolve.
    GitlabTokenRefUnresolvable,
}

impl CredentialMode {
    const fn label(self) -> &'static str {
        match self {
            Self::GithubCliManaged => "github_cli_managed",
            Self::GitlabTokenRefAbsent => "gitlab_token_ref_absent",
            Self::GitlabTokenRefPresent { explicit: true } => "gitlab_token_ref_present_explicit",
            Self::GitlabTokenRefPresent { explicit: false } => "gitlab_token_ref_present_default",
            Self::GitlabTokenRefUnresolvable => "gitlab_token_ref_unresolvable",
        }
    }
}

/// Evidence level for a credential placement row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeEvidence {
    /// The placement follows from configuration only; no credential probe ran.
    ConfiguredNotProbed,
    /// An existing bounded local probe confirmed the placement.
    ProbedOk,
    /// An existing bounded local probe rejected the placement.
    ProbedFailed(String),
}

impl ProbeEvidence {
    /// The evidence wording used in the human-facing placement row.
    pub fn text(&self) -> String {
        match self {
            Self::ConfiguredNotProbed => "configured (not probed)".into(),
            Self::ProbedOk => "probed: ok".into(),
            Self::ProbedFailed(kind) => format!("probed: failed ({kind})"),
        }
    }

    /// The stable machine-facing state. Failure details stay in the text
    /// rendering, so this field remains a small, additive enum.
    pub const fn json_state(&self) -> &'static str {
        match self {
            Self::ConfiguredNotProbed => "not_probed",
            Self::ProbedOk => "ok",
            Self::ProbedFailed(_) => "failed",
        }
    }
}

/// One value-free credential placement row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub provider: Provider,
    pub credential_mode: CredentialMode,
    pub placement: Placement,
    pub reachable: bool,
    pub probe: ProbeEvidence,
    pub note: String,
}

impl Placement {
    pub const ALL: [Self; 3] = [Self::Local, Self::Ssh, Self::Provider];

    const fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
            Self::Provider => "provider",
        }
    }

    const fn is_remote(self) -> bool {
        !matches!(self, Self::Local)
    }
}

impl Provider {
    const fn label(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }
}

/// Classify a resolved GitLab config field without retaining its value.
pub fn gitlab_mode(raw: &str, resolution: &SecretResolution) -> CredentialMode {
    match resolution {
        SecretResolution::Resolved(_) => CredentialMode::GitlabTokenRefPresent {
            explicit: raw != thegn_core::config::GitLabCiConfig::default().token,
        },
        SecretResolution::Failed if raw != thegn_core::config::GitLabCiConfig::default().token => {
            CredentialMode::GitlabTokenRefUnresolvable
        }
        SecretResolution::Absent | SecretResolution::Failed => CredentialMode::GitlabTokenRefAbsent,
    }
}

/// Build one value-free credential placement row.
pub fn report(provider: Provider, credential_mode: CredentialMode, placement: Placement) -> Row {
    let (reachable, detail) = match (provider, credential_mode, placement) {
        (Provider::Github, _, Placement::Local) => (true, "read from host keyring/env; reachable"),
        (Provider::Github, _, Placement::Ssh | Placement::Provider) => {
            (true, "read by remote CLI-managed auth; reachable")
        }
        (Provider::Gitlab, CredentialMode::GitlabTokenRefAbsent, Placement::Local) => {
            (true, "read by host CLI-managed glab login; reachable")
        }
        (Provider::Gitlab, CredentialMode::GitlabTokenRefAbsent, placement)
            if placement.is_remote() =>
        {
            (true, "read by remote CLI-managed glab login; reachable")
        }
        (Provider::Gitlab, CredentialMode::GitlabTokenRefPresent { .. }, Placement::Local) => {
            (true, "read from host env/configured secret; reachable")
        }
        (
            Provider::Gitlab,
            CredentialMode::GitlabTokenRefPresent { explicit: false },
            placement,
        ) if placement.is_remote() => (
            true,
            "not forwarded: remote uses its own glab login; reachable",
        ),
        (Provider::Gitlab, CredentialMode::GitlabTokenRefPresent { explicit: true }, placement)
            if placement.is_remote() =>
        {
            (
                false,
                "refused: configured token is never forwarded to a remote; the remote glab login is not used; unreachable",
            )
        }
        (Provider::Gitlab, CredentialMode::GitlabTokenRefUnresolvable, Placement::Local) => {
            (false, "configured token ref is unresolvable; unreachable")
        }
        (Provider::Gitlab, CredentialMode::GitlabTokenRefUnresolvable, placement)
            if placement.is_remote() =>
        {
            (
                false,
                "configured token ref is unresolvable; remote glab login is not attempted; unreachable",
            )
        }
        (Provider::Gitlab, _, _) => (false, "unsupported CI credential mode; unreachable"),
    };
    Row {
        provider,
        credential_mode,
        placement,
        reachable,
        // Doctor deliberately does not add a network call or a new child
        // process merely to fill in this evidence. An existing bounded local
        // probe can replace this state at the call site when one is available.
        probe: ProbeEvidence::ConfiguredNotProbed,
        note: format!(
            "credential placement: {}/{} — {detail}; {}",
            provider.label(),
            placement.label(),
            ProbeEvidence::ConfiguredNotProbed.text()
        ),
    }
}

impl Row {
    /// Additive JSON representation for `doctor --json` credential rows.
    /// Values and references are intentionally absent.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider.label(),
            "credential_mode": self.credential_mode.label(),
            "placement": self.placement.label(),
            "reachable": self.reachable,
            "note": self.note,
            "probe": self.probe.json_state(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_matrix_is_value_free_and_states_reachability() {
        let secret = "literal-token-configured-in-test";
        let cases = [
            (Provider::Github, CredentialMode::GithubCliManaged),
            (Provider::Gitlab, CredentialMode::GitlabTokenRefAbsent),
            (
                Provider::Gitlab,
                CredentialMode::GitlabTokenRefPresent { explicit: false },
            ),
            (
                Provider::Gitlab,
                CredentialMode::GitlabTokenRefPresent { explicit: true },
            ),
            (Provider::Gitlab, CredentialMode::GitlabTokenRefUnresolvable),
        ];

        for (provider, mode) in cases {
            for placement in Placement::ALL {
                let row = report(provider, mode, placement);
                assert!(!row.note.contains(secret));
                assert!(row.note.contains(placement.label()));
                if matches!(mode, CredentialMode::GitlabTokenRefUnresolvable) {
                    assert!(!row.reachable);
                }
                assert_eq!(row.probe.text(), "configured (not probed)");
                assert_eq!(row.probe.json_state(), "not_probed");
            }
        }

        let remote = report(
            Provider::Gitlab,
            CredentialMode::GitlabTokenRefPresent { explicit: true },
            Placement::Ssh,
        );
        assert!(!remote.reachable);
        assert!(
            remote
                .note
                .contains("configured token is never forwarded to a remote")
        );
    }

    #[test]
    fn gitlab_mode_collapses_resolution_without_exposing_the_value() {
        let absent = gitlab_mode("env:GITLAB_TOKEN", &SecretResolution::Failed);
        assert_eq!(absent, CredentialMode::GitlabTokenRefAbsent);

        let present = gitlab_mode(
            "literal-token-configured-in-test",
            &SecretResolution::Resolved("literal-token-configured-in-test".into()),
        );
        assert_eq!(
            present,
            CredentialMode::GitlabTokenRefPresent { explicit: true }
        );
        let failed = gitlab_mode("keyring:missing", &SecretResolution::Failed);
        assert_eq!(failed, CredentialMode::GitlabTokenRefUnresolvable);
    }

    #[test]
    fn probe_evidence_has_stable_text_and_json_states() {
        let cases = [
            (
                ProbeEvidence::ConfiguredNotProbed,
                "configured (not probed)",
                "not_probed",
            ),
            (ProbeEvidence::ProbedOk, "probed: ok", "ok"),
            (
                ProbeEvidence::ProbedFailed("auth".into()),
                "probed: failed (auth)",
                "failed",
            ),
        ];
        for (evidence, text, json) in cases {
            assert_eq!(evidence.text(), text);
            assert_eq!(evidence.json_state(), json);
        }
    }
}
