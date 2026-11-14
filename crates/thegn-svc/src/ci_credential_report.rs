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

/// One value-free credential placement row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub provider: Provider,
    pub credential_mode: CredentialMode,
    pub placement: Placement,
    pub reachable: bool,
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
        (Provider::Github, _, placement) if placement.is_remote() => {
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
                "not forwarded: remote uses its own glab login; configured token is refused; unreachable",
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
        note: format!(
            "credential placement: {}/{} — {detail}",
            provider.label(),
            placement.label()
        ),
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
                .contains("not forwarded: remote uses its own glab login")
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
}
