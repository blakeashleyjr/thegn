//! `mcp_proxy.profile_env` (b155 THE-2222): a `[mcp_proxy.profiles]` entry's
//! resolved server env, for `thegn mcp proxy --profile` inside the Lead
//! sandbox. Returned only to a Lead-labelled token for `[lead] mcp_profile`,
//! or to an admin caller.
//!
//! A worker has no Lead token, so it cannot obtain a profile's secrets
//! through thegn. Values never reach a log: the audit line names the profile
//! and the caller kind only.

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use thegn_core::config::Config;
use thegn_core::control::Scope;
use thegn_core::control_wire::ProfileEnvRequest;
use thegn_svc::control::{ControlError, ProfileEnvCaller};

/// Answer `mcp_proxy.profile_env` (`{profile}`).
pub(crate) async fn profile_env(
    svc: &DaemonService,
    req: serde_json::Value,
    caller: ProfileEnvCaller,
) -> JsonResult {
    let request: ProfileEnvRequest = decode(req)?;
    let profile = request.profile;
    let kind = authorize(&svc.config, &caller, &profile)?;
    let cfg = std::sync::Arc::clone(&svc.config);
    let name = profile.clone();
    let reply =
        tokio::task::spawn_blocking(move || crate::mcp_proxy::resolve_profile_env(&cfg, &name))
            .await
            .map_err(|error| ControlError::Internal(error.into()))?
            .map_err(|error| ControlError::NotFound(error.to_string()))?;
    tracing::info!(
        profile = %profile,
        caller = kind,
        servers = reply.servers.len(),
        "mcp_proxy.profile_env served"
    );
    encode(&reply)
}

/// Who may read `profile`'s env: an admin caller (any profile), or a
/// Lead-labelled token for exactly `[lead] mcp_profile`. Returns the caller
/// kind for the audit line. Everyone else is refused with `NoScope`, and the
/// refusal is logged with the profile name only.
fn authorize(
    cfg: &Config,
    caller: &ProfileEnvCaller,
    profile: &str,
) -> Result<&'static str, ControlError> {
    if caller.admin {
        return Ok("admin");
    }
    let lead_profile = cfg.lead.as_ref().map(|lead| lead.mcp_profile.as_str());
    let is_lead = caller
        .lead_label
        .as_deref()
        .is_some_and(thegn_core::lead_confine::is_lead_label);
    if is_lead && lead_profile == Some(profile) {
        return Ok("lead");
    }
    tracing::info!(
        profile = %profile,
        lead = is_lead,
        "mcp_proxy.profile_env refused: the profile is bound to the Lead"
    );
    Err(ControlError::NoScope { need: Scope::Admin })
}

#[cfg(test)]
mod tests {
    use super::authorize;
    use thegn_core::config::Config;
    use thegn_svc::control::{ControlError, ProfileEnvCaller};

    fn cfg_with_lead() -> Config {
        let mut cfg = Config::default();
        cfg.lead = Some(thegn_core::config_lead::LeadConfig::default());
        cfg
    }

    fn caller(lead_label: Option<&str>, admin: bool) -> ProfileEnvCaller {
        ProfileEnvCaller {
            lead_label: lead_label.map(str::to_string),
            admin,
        }
    }

    fn refused(r: Result<&'static str, ControlError>) -> bool {
        matches!(r, Err(ControlError::NoScope { .. }))
    }

    #[test]
    fn profile_env_authorization_table() {
        let cfg = cfg_with_lead();
        // Admin: any profile, even with no [lead].
        assert_eq!(
            authorize(&cfg, &caller(None, true), "lead").unwrap(),
            "admin"
        );
        assert_eq!(
            authorize(&cfg, &caller(None, true), "other").unwrap(),
            "admin"
        );
        assert_eq!(
            authorize(&Config::default(), &caller(None, true), "x").unwrap(),
            "admin"
        );
        // Lead token for its own profile (bare and per-tab labels).
        assert_eq!(
            authorize(&cfg, &caller(Some("lead"), false), "lead").unwrap(),
            "lead"
        );
        assert_eq!(
            authorize(&cfg, &caller(Some("lead:lead/0"), false), "lead").unwrap(),
            "lead"
        );
        // Lead token for another profile: refused.
        assert!(refused(authorize(
            &cfg,
            &caller(Some("lead"), false),
            "other"
        )));
        // A non-Lead label that slipped through confinement: refused.
        assert!(refused(authorize(
            &cfg,
            &caller(Some("phone"), false),
            "lead"
        )));
        // Plain (unconfined, non-admin) token: refused.
        assert!(refused(authorize(&cfg, &caller(None, false), "lead")));
        // No [lead] configured: a Lead token gets nothing.
        assert!(refused(authorize(
            &Config::default(),
            &caller(Some("lead"), false),
            "lead"
        )));
    }
}
