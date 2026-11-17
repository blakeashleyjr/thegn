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

use super::http::{ControlState, RequestAuth, authed_target, bad_request};
use super::{ControlApi, ControlError, ControlResult};

/// One Lead verb's trait method, as a plain function pointer (non-capturing
/// closures coerce to it), so one helper serves all eleven routes.
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
);

#[cfg(test)]
mod tests {
    use super::lead_target;
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
