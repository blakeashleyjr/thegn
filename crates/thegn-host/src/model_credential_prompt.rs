//! The one-time model-credential question and its answers.
//!
//! The prompt is raised by the spec drain when
//! [`crate::agent::provision_provider_env_inner`] refuses an unset Sprites env:
//! the drain cannot ask (it is off the loop), so it lowers the typed error and
//! the focused tab's batch shows this menu. Answers are per project+provider;
//! "not now" is a one-shot in-process veto. Durable writes ride the DB writer
//! thread so the event loop never blocks on SQLite.

use crate::menu::ModelCredentialTarget;
use thegn_core::model_credential_consent::{self as consent, Consent};

/// Build the menu for a refused launch on `group`/`tab`.
pub(crate) fn menu_for(
    prompt: &crate::agent::ModelCredentialConsentRequired,
    group: &str,
    tab: usize,
) -> crate::menu::MenuOverlay {
    let target = ModelCredentialTarget {
        project: prompt.project.clone(),
        provider: prompt.provider.clone(),
        worktree: prompt.worktree.clone(),
        group: group.to_string(),
        tab,
    };
    crate::menu::model_credentials_menu(crate::agent::model_credential_question(prompt), target)
}

/// Forward and remember for this project+provider.
pub(crate) fn remember_forward(target: &ModelCredentialTarget) {
    remember(target, Consent::Forward);
}

/// Never forward for this project+provider.
pub(crate) fn remember_never(target: &ModelCredentialTarget) {
    remember(target, Consent::Never);
}

/// Withhold for this launch only; the question returns next launch.
pub(crate) fn not_now(target: &ModelCredentialTarget) {
    crate::agent::withhold_model_credentials_once(&target.provider, &target.worktree);
}

fn remember(target: &ModelCredentialTarget, answer: Consent) {
    // Memoize both scopes so the immediate retry (and the pane path) sees the
    // answer before the writer thread drains.
    crate::agent::memo_model_credential_consent(&target.provider, &target.project, answer);
    crate::agent::memo_model_credential_consent(&target.provider, &target.worktree, answer);
    // The gap between the menu selection and the write is short, but the
    // answer is a user decision: do not lose it to a best-effort cap.
    let project = target.project.clone();
    let provider = target.provider.clone();
    crate::db_task::persist_critical(move |db| {
        if let Err(error) = consent::remember(db, &project, &provider, answer) {
            tracing::warn!(
                target: "thegn::provider",
                %error,
                "could not remember the model-credential answer"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> crate::agent::ModelCredentialConsentRequired {
        crate::agent::ModelCredentialConsentRequired {
            env_name: "cloud".into(),
            project: "/repo".into(),
            provider: "sprites".into(),
            worktree: "/repo/wt".into(),
            sandbox: "wt-abc".into(),
            agents: vec!["claude".into()],
            keys: vec!["ANTHROPIC_API_KEY".into()],
        }
    }

    #[test]
    fn menu_names_agents_keys_and_sandbox_but_no_values() {
        let menu = menu_for(&prompt(), "repo", 2);
        let labels: Vec<&str> = menu.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "forward, remember for this project",
                "not now",
                "never for this project",
            ]
        );
        let body = crate::agent::model_credential_question(&prompt());
        assert!(body.contains("claude login"));
        assert!(body.contains("ANTHROPIC_API_KEY"));
        assert!(body.contains("wt-abc"));
        assert!(!body.contains("sk-"));
    }
}
