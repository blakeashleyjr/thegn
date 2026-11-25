//! Per-project consent for forwarding model credentials into a provider
//! sandbox.
//!
//! Sprites used to forward the fixed model-credential env names and the agent
//! login files whenever `model_credentials` was omitted. THE-2448 replaces that
//! default with one question per project+provider: forward and remember, not
//! now (this launch only), or never. The answer lives in `ui_state`, next to
//! thegn's other per-project UI grants, so no config write is needed.
//!
//! This module is storage + policy only. The host decides whether a prompt can
//! be shown (`msg::tui_active`) and renders the menu; non-interactive callers
//! withhold and let the existing provisioning notice name what was withheld.

use crate::config::ModelCredentials;
use crate::db::Db;
use crate::store::WorkspaceStore;

/// `ui_state` scope prefix. The provider is part of the scope so a project can
/// answer independently for Sprites and Vercel.
pub const SCOPE_PREFIX: &str = "model_credentials_consent";

/// The durable answer for one project+provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Forward the model env keys and agent login files.
    Forward,
    /// Never forward them for this project+provider.
    Never,
}

impl Consent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Never => "never",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "forward" => Some(Self::Forward),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// What a launch should do with model credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// Send the fixed model env keys and the agent login files.
    Forward,
    /// Do not send them.
    Withhold,
    /// No remembered answer and a UI is available: ask.
    Ask,
}

/// Resolve the effective launch policy.
///
/// `explicit` is whether the user wrote `model_credentials` in config. An
/// explicit `upload` forwards; an explicit `none` (or a refused `proxy`/
/// `inject`) withholds. When the key is unset, only `upload` forwards without a
/// question; `none` consults the remembered answer, asking only when a UI is
/// available. No remember and no UI withholds, never hangs.
pub fn decide(
    mode: ModelCredentials,
    explicit: bool,
    consent: Option<Consent>,
    interactive: bool,
) -> Launch {
    if explicit {
        return match mode {
            ModelCredentials::Upload => Launch::Forward,
            _ => Launch::Withhold,
        };
    }
    match mode {
        ModelCredentials::Upload => Launch::Forward,
        ModelCredentials::None => match consent {
            Some(Consent::Forward) => Launch::Forward,
            Some(Consent::Never) => Launch::Withhold,
            None if interactive => Launch::Ask,
            None => Launch::Withhold,
        },
        ModelCredentials::Proxy | ModelCredentials::Inject => Launch::Withhold,
    }
}

/// The `ui_state` scope for `provider`.
pub fn scope(provider: &str) -> String {
    format!("{SCOPE_PREFIX}:{}", provider.trim().to_ascii_lowercase())
}

/// The remembered answer for `project`+`provider`, if any. A corrupt stored
/// value is treated as absent: ask (or withhold headless) rather than guess.
pub fn load(db: &Db, project: &str, provider: &str) -> anyhow::Result<Option<Consent>> {
    let Some(value) = db.get_ui_state(&scope(provider), project)? else {
        return Ok(None);
    };
    Ok(Consent::parse(&value))
}

/// Remember `consent` for `project`+`provider`.
pub fn remember(db: &Db, project: &str, provider: &str, consent: Consent) -> anyhow::Result<()> {
    db.set_ui_state(&scope(provider), project, consent.as_str())
}

/// Forget the remembered answer (used by tests and any future UI reset).
pub fn forget(db: &Db, project: &str, provider: &str) -> anyhow::Result<()> {
    db.del_ui_state(&scope(provider), project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_asks_with_a_ui_and_withholds_headless() {
        assert_eq!(
            decide(ModelCredentials::None, false, None, true),
            Launch::Ask
        );
        assert_eq!(
            decide(ModelCredentials::None, false, None, false),
            Launch::Withhold
        );
    }

    #[test]
    fn explicit_modes_skip_the_question() {
        assert_eq!(
            decide(ModelCredentials::Upload, true, None, true),
            Launch::Forward
        );
        assert_eq!(
            decide(ModelCredentials::None, true, None, true),
            Launch::Withhold
        );
        assert_eq!(
            decide(ModelCredentials::Proxy, true, None, true),
            Launch::Withhold
        );
    }

    #[test]
    fn remembered_answers_skip_the_question() {
        assert_eq!(
            decide(ModelCredentials::None, false, Some(Consent::Forward), true),
            Launch::Forward
        );
        assert_eq!(
            decide(ModelCredentials::None, false, Some(Consent::Never), true),
            Launch::Withhold
        );
    }

    #[test]
    fn consent_is_per_project_and_provider() {
        let db = Db::open_memory().expect("in-memory db");
        assert_eq!(load(&db, "/repo", "sprites").expect("load"), None);
        remember(&db, "/repo", "sprites", Consent::Forward).expect("remember");
        remember(&db, "/repo", "vercel", Consent::Never).expect("remember");
        assert_eq!(
            load(&db, "/repo", "sprites").expect("load"),
            Some(Consent::Forward)
        );
        assert_eq!(
            load(&db, "/repo", "vercel").expect("load"),
            Some(Consent::Never)
        );
        assert_eq!(load(&db, "/other", "sprites").expect("load"), None);
        forget(&db, "/repo", "sprites").expect("forget");
        assert_eq!(load(&db, "/repo", "sprites").expect("load"), None);
    }
}
