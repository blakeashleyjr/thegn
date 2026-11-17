//! HTTP adapters for the Lead tool surface (THE-2172).
//!
//! Every route is a `POST` with a JSON object body, reads included (the
//! `automations.test` / `preview.fetch` precedent), so the generic client's
//! `build_call` carries typed bodies unchanged. The [`super::ControlApi`]
//! methods take and return `serde_json::Value`; the typed DTOs live in each
//! verb's owning thegn-core module. These handlers only authenticate, check
//! the body is an object, and forward.
//!
//! Confinement: a provider-injected route-to-host token (the only
//! target-confined credential, see
//! [`thegn_core::control::RouteToHostTokenBinding`]) is bound to one worktree
//! and is never a Lead caller, so every Lead verb refuses it. `merge.drain`
//! folds a whole repo's queue, so an unbound caller needs `git` (or admin),
//! exactly as the MCP gate for `merge.add`.

use axum::{
    extract::State,
    response::{IntoResponse, Response},
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use thegn_core::control::{RouteToHostTokenBinding, Scope, Verb};

use super::http::{ControlState, RequestAuth, authed_shaped, authed_target, bad_request};
use super::{ControlApi, ControlError, ControlResult, ProfileEnvCaller};

/// One Lead verb's trait method, as a plain function pointer (non-capturing
/// closures coerce to it), so one helper serves every Lead route.
type LeadCall = for<'a> fn(&'a dyn ControlApi, Value) -> BoxFuture<'a, ControlResult<Value>>;

/// The audit / authorization target for a Lead body: `worktree` when present,
/// else `name` for the automation verbs, else `""`.
fn lead_target(verb: Verb, body: &Value) -> String {
    if let Some(worktree) = body.get("worktree").and_then(Value::as_str) {
        return worktree.to_string();
    }
    let automation = matches!(
        verb,
        Verb::AutomationsPut | Verb::AutomationsRm | Verb::AutomationsEnable
    );
    if automation && let Some(name) = body.get("name").and_then(Value::as_str) {
        return name.to_string();
    }
    String::new()
}

/// Authenticate, refuse confined tokens and non-object bodies, then call the
/// verb's trait method and serialize its reply.
async fn lead_post(
    state: &ControlState,
    headers: &RequestAuth,
    verb: Verb,
    body: Value,
    call: LeadCall,
) -> Response {
    let target = lead_target(verb, &body);
    let ctx = match authed_target(state, headers, verb, &target) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if RouteToHostTokenBinding::parse_label(&ctx.label).is_some() {
        return ControlError::FailedPrecondition("confined token".into()).into_response();
    }
    if matches!(verb, Verb::MergeDrain)
        && !thegn_core::control::route_to_host_token_allows_worktree(
            ctx.scopes, &ctx.label, &target,
        )
    {
        return ControlError::NoScope { need: Scope::Git }.into_response();
    }
    if !body.is_object() {
        return bad_request("request body must be a JSON object");
    }
    match call(state.api.as_ref(), body).await {
        Ok(reply) => axum::Json(reply).into_response(),
        Err(error) => error.into_response(),
    }
}

/// Declare one `POST` handler per Lead verb: `(handler, Verb, trait method)`.
macro_rules! lead_handlers {
    ($(($handler:ident, $verb:ident, $method:ident)),+ $(,)?) => {
        $(
            pub(super) async fn $handler(
                State(state): State<ControlState>,
                headers: RequestAuth,
                body: axum::Json<Value>,
            ) -> Response {
                lead_post(&state, &headers, Verb::$verb, body.0, |api, req| api.$method(req)).await
            }
        )+
    };
}

lead_handlers!(
    (overview_get, OverviewGet, overview_get),
    (notifications_list, NotificationsList, notifications_list),
    (notifications_ack, NotificationsAck, notifications_ack),
    (automations_put, AutomationsPut, automations_put),
    (automations_rm, AutomationsRm, automations_rm),
    (automations_enable, AutomationsEnable, automations_enable),
    (automations_runs, AutomationsRuns, automations_runs),
    (approvals_list, ApprovalsList, approvals_list),
    (approvals_request, ApprovalsRequest, approvals_request),
    (approvals_resolve, ApprovalsResolve, approvals_resolve),
    (merge_drain, MergeDrain, merge_drain),
    // THE-2197 kill switch: pause is Write; resume is Admin (operator only).
    (automations_pause, AutomationsPause, automations_pause),
    (automations_resume, AutomationsResume, automations_resume),
    // b155: the phone's answer (Write; the Lead is refused) and the
    // operator's kill switch (Admin).
    (approvals_redeem, ApprovalsRedeem, approvals_redeem),
    (automations_halt, AutomationsHalt, automations_halt),
);

/// `mcp_proxy.profile_env` (THE-2222). Read scope; a Lead-confined caller
/// passes the transport as [`thegn_core::lead_confine::RequestShape::DaemonGated`]
/// and the daemon decides from [`ProfileEnvCaller`], which is built here from
/// the verified token only.
pub(super) async fn mcp_proxy_profile_env(
    State(state): State<ControlState>,
    headers: RequestAuth,
    body: axum::Json<Value>,
) -> Response {
    let ctx = match authed_shaped(
        &state,
        &headers,
        Verb::McpProxyProfileEnv,
        "",
        &thegn_core::lead_confine::RequestShape::DaemonGated,
    ) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if RouteToHostTokenBinding::parse_label(&ctx.label).is_some() {
        return ControlError::FailedPrecondition("confined token".into()).into_response();
    }
    if !body.is_object() {
        return bad_request("request body must be a JSON object");
    }
    let caller = profile_env_caller(&ctx);
    match state.api.mcp_proxy_profile_env(body.0, caller).await {
        Ok(reply) => axum::Json(reply).into_response(),
        Err(error) => error.into_response(),
    }
}

/// The trusted caller facts for `mcp_proxy.profile_env`.
fn profile_env_caller(ctx: &super::auth::AuthCtx) -> ProfileEnvCaller {
    ProfileEnvCaller {
        lead_label: ctx.lead_confined.then(|| ctx.label.clone()),
        admin: !ctx.lead_confined && ctx.scopes.allows(Scope::Admin),
    }
}

#[cfg(test)]
mod tests {
    use super::{lead_target, profile_env_caller};
    use crate::control::auth::AuthCtx;
    use thegn_core::control::ScopeSet;

    #[test]
    fn profile_env_caller_comes_from_the_token_only() {
        let admin = profile_env_caller(&AuthCtx::local_admin());
        assert!(admin.admin && admin.lead_label.is_none());
        let lead = profile_env_caller(&AuthCtx {
            pairing_id: "p".into(),
            label: "lead:g/0".into(),
            scopes: ScopeSet::parse("read,write"),
            lead_confined: true,
        });
        assert_eq!(lead.lead_label.as_deref(), Some("lead:g/0"));
        assert!(!lead.admin);
        let phone = profile_env_caller(&AuthCtx {
            pairing_id: "p".into(),
            label: "phone".into(),
            scopes: ScopeSet::parse("read"),
            lead_confined: false,
        });
        assert!(!phone.admin && phone.lead_label.is_none());
        // A confined caller is never admin, even with an admin grant.
        let odd = profile_env_caller(&AuthCtx {
            pairing_id: "p".into(),
            label: "lead".into(),
            scopes: ScopeSet::parse("admin"),
            lead_confined: true,
        });
        assert!(!odd.admin);
    }
    use serde_json::json;
    use thegn_core::control::Verb;

    #[test]
    fn lead_target_prefers_worktree_then_automation_name() {
        let both = json!({ "worktree": "/w", "name": "r" });
        assert_eq!(lead_target(Verb::AutomationsRm, &both), "/w");
        assert_eq!(
            lead_target(Verb::AutomationsEnable, &json!({ "name": "r" })),
            "r"
        );
        // `name` only names a target for the automation verbs.
        assert_eq!(lead_target(Verb::OverviewGet, &json!({ "name": "r" })), "");
        assert_eq!(lead_target(Verb::NotificationsList, &json!([])), "");
        assert_eq!(lead_target(Verb::MergeDrain, &json!({ "worktree": 3 })), "");
    }
}
