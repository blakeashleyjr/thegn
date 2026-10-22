//! Loop-side plugin state + drain handler.
//!
//! The off-loop host ([`crate::plugins`]) discovers plugins, runs their
//! processes and forwards their NDJSON messages here; this module owns the
//! per-plugin [`PluginRuntime`] state machines and applies verbs at drain
//! time. Everything on the drain path is non-blocking: notification records
//! are flushed off-loop by [`flush_alerts`], `host.call` dispatch happens on
//! the [`Dispatcher`] thread (which answers the plugin directly through its
//! [`SessionWriter`], never re-entering the loop), and respawns are scheduled
//! through [`crate::plugins::PluginsHost::respawn`].

use std::collections::BTreeMap;
use std::sync::Arc;

use thegn_core::plugin_api::{
    API_VERSION, Alert, AuditDecision, Capability, Contribution, Event, EventKind, ExtensionPoint,
    HOST_VERB_SUPPORT, HostVerb, IoRequest, NegotiatedManifest, PluginApiError, PluginId,
    PluginMode, PluginRuntime, PluginSpec, RpcError, RpcErrorCode, RpcMessage, RpcResponse,
    SupportState, SurfaceId, View,
};
use thegn_svc::issue::IssueCaps;
use thegn_svc::plugin::session::SessionFailure;
use thegn_svc::plugin::{LoadedPlugin, SessionEvent, SessionWriter};

use crate::chrome::FrameModel;
use crate::plugin_damage::PluginDamage;
use crate::plugins::inbox::{PluginInbox, RejectedRequest};
use crate::plugins::{LoadedState, OneShotStart, PluginMsg, PluginRx, PluginsHost};

const MAX_PENDING_ALERTS_PER_PLUGIN: usize = 16;
// The inbox/toast row is display text, so keep its stricter host-side cap.
const MAX_ALERT_MESSAGE_BYTES: usize = 1024;
const _: () = assert!(MAX_ALERT_MESSAGE_BYTES <= thegn_core::plugin_api::MAX_ALERT_MESSAGE_BYTES);
const MAX_FEED_BRIDGES: usize = 8;
const MAX_SUBSCRIPTIONS_PER_PLUGIN: usize = thegn_core::plugin_api::MAX_PLUGIN_SUBSCRIPTIONS;
const FEED_BRIDGE_THREAD: &str = "thegn-plugfeed";
const MAX_PLUGIN_MESSAGES_PER_DRAIN: usize = 256;
const DISPATCH_QUEUE_CAPACITY: usize = 64;
const MAX_DISPATCH_IN_FLIGHT_PER_PLUGIN: usize = 8;
const DISPATCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Owns a feed worker and its cancellation signal. Dropping the owner cancels
/// the worker even if the feed is idle and the plugin never reads another frame.
pub(crate) struct FeedBridge {
    handle: Arc<std::thread::JoinHandle<()>>,
    _cancel: tokio::sync::oneshot::Sender<()>,
}

/// One live (or dead-but-remembered) plugin on the loop side.
pub(crate) struct PluginEntry {
    pub plugin: LoadedPlugin,
    pub runtime: PluginRuntime,
    /// `Some` while a resident session is alive; `None` for one-shot plugins
    /// and dead residents.
    pub writer: Option<SessionWriter>,
    /// A live feed bridge, if one was started for this resident generation.
    pub feed_bridge: Option<FeedBridge>,
    /// Contributions currently in effect, selected during manifest negotiation.
    /// Runtime `register` messages confirm these declarations but cannot add to them.
    pub contributions: Vec<Contribution>,
    /// How many restarts have been *attempted* (drives the backoff schedule).
    pub restarts: u32,
    /// Restart cap reached: dead until a config reload rebuilds the state.
    pub disabled: bool,
    /// Live `provider.call` correlation for resident plugins with provider
    /// contributions (`ExtensionPoint::IssueProvider`); `None` otherwise.
    pub bridge: Option<std::sync::Arc<thegn_svc::plugin::ProviderBridge>>,
}

/// Loop-side plugin state: entries by plugin id (BTreeMap ⇒ stable statusbar
/// order) plus the outboxes the loop flushes off-thread after each drain.
pub(crate) struct PluginsState {
    pub plugins: BTreeMap<String, PluginEntry>,
    /// Notifications raised via the `notify` verb this drain, recorded to the
    /// inbox off-loop by [`flush_alerts`]. `(plugin id, alert)`.
    pub pending_alerts: Vec<(String, Alert)>,
    /// Number of alerts dropped per plugin in the current outbox batch.
    pending_alert_drops: BTreeMap<String, usize>,
    inbox: Option<Arc<PluginInbox>>,
    /// Lazily started `host.call` dispatcher (a thread is only spawned once a
    /// plugin actually calls).
    dispatcher: Option<Dispatcher>,
    /// Shared config for the dispatcher's daemon discovery and feed bridges.
    cfg: Arc<thegn_core::config::Config>,
}

impl PluginsState {
    pub(crate) fn new(cfg: thegn_core::config::Config) -> Self {
        Self {
            plugins: BTreeMap::new(),
            pending_alerts: Vec::new(),
            pending_alert_drops: BTreeMap::new(),
            inbox: None,
            dispatcher: None,
            cfg: Arc::new(cfg),
        }
    }

    /// Lazily start the host.call dispatcher, sharing the config without
    /// cloning the full value on each request.
    fn dispatcher(&mut self) -> &mut Dispatcher {
        self.dispatcher
            .get_or_insert_with(|| Dispatcher::spawn(Arc::clone(&self.cfg)))
    }
}

/// Scope-check one `host.call` capability id against a plugin's granted scope
/// set, through the same catalog + `required_scope` lattice every other door
/// uses. Pure; unit-tested.
pub(crate) fn host_call_check(spec: &PluginSpec, cap: &str) -> Result<(), RpcError> {
    let Some(row) = thegn_core::capability::CATALOG
        .iter()
        .find(|r| r.id.as_str() == cap)
    else {
        return Err(RpcError::new(
            RpcErrorCode::Invalid,
            format!("unknown capability {cap}"),
        ));
    };
    if !row
        .surfaces
        .contains(thegn_core::capability::Surface::Plugin)
    {
        return Err(RpcError::new(
            RpcErrorCode::Unsupported,
            format!("{cap} is not exposed on the plugin surface"),
        ));
    }
    let need = thegn_core::control::required_scope(row.verb);
    if spec.scope_set().allows(need) {
        if row.verb == thegn_core::control::Verb::MergeAdd
            && !thegn_core::control::route_to_host_token_allows_worktree(spec.scope_set(), "", "")
        {
            // An empty label means there is no route binding; the worktree
            // argument is unused by this pure scope predicate.
            return Err(RpcError::new(
                RpcErrorCode::Denied,
                "merge.add requires a worktree-bound route-to-host grant; plugins need the git scope",
            ));
        }
        Ok(())
    } else {
        Err(RpcError::new(
            RpcErrorCode::Denied,
            format!("{cap} requires the {need:?} scope, which this plugin was not granted"),
        ))
    }
}

/// Enforce the canonical verb support/mode row before dispatch. A one-shot
/// process can print any valid wire verb, but resident-only request/reply verbs
/// must not become accidentally supported merely because decoding succeeds.
fn host_verb_check(spec: &PluginSpec, verb: HostVerb) -> Result<(), RpcError> {
    let row = HOST_VERB_SUPPORT
        .iter()
        .find(|row| row.verb == verb)
        .expect("HostVerb::ALL and HOST_VERB_SUPPORT are pinned to the same set");
    if row.state == SupportState::Wired && row.modes.contains(&spec.mode) {
        return Ok(());
    }
    Err(RpcError::new(
        RpcErrorCode::Unsupported,
        format!(
            "{} is not supported for {} plugins; supported modes: {}",
            verb.method_name(),
            spec.mode.wire_name(),
            row.modes
                .iter()
                .map(|mode| mode.wire_name())
                .collect::<Vec<_>>()
                .join(",")
        ),
    ))
}

/// Apply the pure scope decision and record it in the plugin runtime's audit
/// stream before any control dispatch. Both successful and denied calls use
/// this path, including unknown or deliberately unexposed capabilities.
fn authorize_host_call(
    entry: &mut PluginEntry,
    cap: &str,
    params: &serde_json::Value,
) -> Result<(), RpcError> {
    let result = host_call_check(&entry.plugin.spec, cap);
    let target = (cap == "merge.add")
        .then(|| params.get("worktree").and_then(serde_json::Value::as_str))
        .flatten();
    entry.runtime.record_host_call_decision(
        entry.plugin.spec.manifest.id.clone(),
        cap,
        if result.is_ok() {
            AuditDecision::Granted
        } else {
            AuditDecision::Denied
        },
        target,
    );
    result
}

/// The grant set a plugin's [`PluginRuntime`] is built with. The negotiated
/// grants (empty today — the host contract grants nothing wholesale) are
/// widened with the host-side policy for what a loaded plugin may always do:
/// drive the surfaces of its accepted contributions, keep its own namespaced
/// state, and — when a `NotificationSource` contribution was accepted — raise
/// alerts under its own id (plus any `notify:*` capability it declared).
pub(crate) fn effective_grants(
    neg: &NegotiatedManifest,
    spec: &PluginSpec,
) -> std::collections::HashSet<Capability> {
    let mut grants = neg.granted.clone();
    let id = spec.manifest.id.as_str();
    grants.insert(Capability::new("state", id));
    let mut notification_source = false;
    for c in &neg.accepted_contributions {
        match c.extension_point {
            ExtensionPoint::StatusBarSegment => {
                grants.insert(Capability::new("surface", "statusbar"));
            }
            ExtensionPoint::NotificationSource => {
                grants.insert(Capability::new("surface", "notification"));
                notification_source = true;
            }
            _ => {}
        }
    }
    if notification_source {
        grants.insert(Capability::new("notify", id));
        for cap in &spec.manifest.capabilities {
            if cap.kind() == "notify" {
                grants.insert(cap.clone());
            }
        }
    }
    grants
}

/// Build one loop-side entry from a discovery result.
fn build_entry(st: LoadedState) -> PluginEntry {
    let mut negotiated = st.negotiated;
    negotiated.granted = effective_grants(&negotiated, &st.plugin.spec);
    let contributions = negotiated.accepted_contributions.clone();
    let bridge = st.writer.as_ref().and_then(|w| {
        contributions
            .iter()
            .any(|c| c.extension_point == ExtensionPoint::IssueProvider)
            .then(|| {
                thegn_svc::plugin::ProviderBridge::new(w.clone(), st.plugin.spec.run_timeout())
            })
    });
    PluginEntry {
        runtime: PluginRuntime::new(negotiated),
        plugin: st.plugin,
        writer: st.writer,
        feed_bridge: None,
        contributions,
        restarts: 0,
        disabled: false,
        bridge,
    }
}

/// Publish the live issue-provider bridges to the process-global registry
/// the hydration workers read (`crate::plugin_providers`). Called after any
/// event that changes the live set (load, exit, respawn, disable).
fn sync_provider_registry(state: &PluginsState) {
    let rows = state
        .plugins
        .iter()
        .filter(|(_, e)| !e.disabled)
        .filter_map(|(id, e)| {
            let bridge = e.bridge.clone()?;
            let label = e
                .contributions
                .iter()
                .find(|c| c.extension_point == ExtensionPoint::IssueProvider)
                .map(|c| c.label.clone())
                .unwrap_or_else(|| id.clone());
            let caps = e
                .contributions
                .iter()
                .find(|c| c.extension_point == ExtensionPoint::IssueProvider)
                .map(|c| {
                    IssueCaps::from_json(&c.caps).unwrap_or_else(|error| {
                        tracing::warn!(
                            plugin = %id,
                            error = %error,
                            "invalid issue provider caps; treating optional operations as unsupported"
                        );
                        IssueCaps::default()
                    })
                })
                .unwrap_or_default();
            Some((id.clone(), label, bridge, caps))
        })
        .collect();
    crate::plugin_providers::set_issue_providers(rows);
}

/// Send the `activate` callback to a freshly (re)started resident session:
/// the negotiated api version plus the effective grant list, sorted for
/// determinism.
fn send_activate(entry: &PluginEntry) {
    let Some(writer) = &entry.writer else { return };
    let mut granted: Vec<String> = effective_grants_of(entry)
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();
    granted.sort();
    // best-effort: a session that died between spawn and activate surfaces
    // via its Exit event.
    // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
    let _ = writer.notify(
        thegn_core::plugin_api::PluginCallback::Activate,
        serde_json::json!({ "api": API_VERSION, "granted": granted }),
    );
}

/// Recompute the grant set for activation params (the runtime keeps its copy
/// private, so re-derive from the same pure policy).
fn effective_grants_of(entry: &PluginEntry) -> std::collections::HashSet<Capability> {
    // Reconstruct a minimal NegotiatedManifest view: accepted contributions
    // are what the grant policy reads.
    let neg = NegotiatedManifest {
        accepted_contributions: entry.contributions.clone(),
        ..Default::default()
    };
    effective_grants(&neg, &entry.plugin.spec)
}

/// The statusbar segments to render: `(label, cached view)` for every
/// accepted `StatusBarSegment` contribution that has produced a view, in
/// stable (plugin id, contribution) order.
pub(crate) fn statusbar_views(state: &PluginsState) -> Vec<(String, View)> {
    let mut out = Vec::new();
    for entry in state.plugins.values() {
        if entry.disabled {
            continue;
        }
        for c in &entry.contributions {
            if c.extension_point != ExtensionPoint::StatusBarSegment {
                continue;
            }
            let Some(surface) = &c.surface else { continue };
            if let Some(view) = entry.runtime.view(surface) {
                out.push((c.label.clone(), view.clone()));
            }
        }
    }
    out
}

/// Palette rows contributed by plugins: one per accepted `PaletteAction`
/// contribution, keyed `plugin:<plugin>:<contribution>` — a namespaced,
/// contract-negotiated class the dispatch arm handles before `Action`
/// lookup, so the "every palette key is an Action" invariant keeps holding
/// for everything else.
pub(crate) fn palette_items(state: &PluginsState) -> Vec<crate::palette::PaletteItem> {
    let mut out = Vec::new();
    for (id, entry) in &state.plugins {
        if entry.disabled {
            continue;
        }
        for c in &entry.contributions {
            if c.extension_point != ExtensionPoint::PaletteAction {
                continue;
            }
            out.push(crate::palette::PaletteItem::new(
                format!("plugin:{id}:{}", c.id.as_str()),
                format!("\u{2699} {}  (plugin: {id})", c.label),
            ));
        }
    }
    out
}

/// Invoke a plugin palette action by its `plugin:<plugin>:<contribution>`
/// key. Dispatch follows the configured plugin mode: a live resident gets an
/// `on_event` notification (`kind: Action`, `payload.id` = the contribution),
/// while a one-shot plugin runs once via
/// [`crate::plugins::PluginsHost::run_one_shot`]. A resident without a live
/// writer reports that it is unavailable and is never run as one-shot.
pub(crate) fn invoke_palette_action(
    state: &PluginsState,
    host: Option<&crate::plugins::PluginsHost>,
    key: &str,
) -> Option<String> {
    let rest = key.strip_prefix("plugin:")?;
    let (plugin, contribution) = rest.split_once(':')?;
    let entry = state.plugins.get(plugin)?;
    if entry.disabled {
        return Some(format!("Plugin {plugin} is disabled (restart cap)"));
    }
    let owns = entry.contributions.iter().any(|c| {
        c.extension_point == ExtensionPoint::PaletteAction && c.id.as_str() == contribution
    });
    if !owns {
        return Some(format!("Plugin {plugin} has no action {contribution}"));
    }
    let event = thegn_core::plugin_api::Event::new(
        thegn_core::plugin_api::EventKind::Action,
        serde_json::json!({ "id": contribution }),
    );
    match entry.plugin.spec.mode {
        PluginMode::Resident => match &entry.writer {
            Some(writer) => {
                if let Err(e) = writer.notify(
                    thegn_core::plugin_api::PluginCallback::OnEvent,
                    serde_json::to_value(&event).unwrap_or_default(),
                ) {
                    return Some(format!("Plugin {plugin}: {e}"));
                }
                Some(format!("Plugin {plugin}: {contribution}"))
            }
            None => Some(format!("Plugin {plugin} is not running (restarting)")),
        },
        PluginMode::OneShot => {
            let start = host?.run_one_shot(entry.plugin.clone());
            Some(one_shot_status(plugin, contribution, start))
        }
    }
}

fn one_shot_status(plugin: &str, contribution: &str, start: OneShotStart) -> String {
    match start {
        OneShotStart::Started => format!("Plugin {plugin}: running {contribution}"),
        OneShotStart::Busy => format!("Plugin {plugin}: {contribution} is already running"),
        OneShotStart::Overloaded => format!(
            "Plugin {plugin}: {contribution} not started: too many plugin runs in flight ({} max)",
            crate::plugins::MAX_INFLIGHT_ONE_SHOTS
        ),
        OneShotStart::Failed => format!("Plugin {plugin}: could not start {contribution}"),
    }
}

/// Record this drain's raised alerts to the notification inbox, off-loop.
/// Call after [`drain`] from the event loop. One job per non-empty drain is
/// sent to the shared DB writer, which counts queue overflow.
pub(crate) fn flush_alerts(state: &mut PluginsState) {
    flush_alerts_with(state, |alerts| {
        crate::db_task::persist(move |db| persist_alert_batch(db, alerts));
    });
}

fn persist_alert_batch(db: &thegn_core::db::Db, alerts: Vec<(String, Alert)>) {
    for (plugin, alert) in alerts {
        if let Err(error) = crate::automation_events::emit(
            db,
            "plugin",
            &plugin,
            &format!("{}: {}", alert.source, alert.message),
            "",
        ) {
            tracing::debug!(target: "thegn::plugin", plugin = %plugin, error = %error, "plugin alert persistence failed");
        }
    }
}

fn flush_alerts_with(state: &mut PluginsState, submit: impl FnOnce(Vec<(String, Alert)>)) {
    if let Some(dispatcher) = &state.dispatcher {
        for (plugin, counts) in dispatcher.take_diagnostics() {
            if counts.rejected > 0
                || counts.timed_out > 0
                || counts.unavailable > 0
                || counts.abandoned > 0
                || counts.undelivered > 0
            {
                tracing::warn!(target: "thegn::plugin", plugin = %plugin, rejected = counts.rejected, timed_out = counts.timed_out, unavailable = counts.unavailable, abandoned = counts.abandoned, undelivered = counts.undelivered, "plugin host.call dispatcher pressure");
            }
        }
    }
    if let Some(inbox) = &state.inbox {
        for (plugin, counts) in inbox.take_diagnostics() {
            if counts.dropped > 0 || counts.rejected > 0 {
                tracing::warn!(target: "thegn::plugin", plugin = %plugin, coalesced = counts.coalesced, dropped = counts.dropped, rejected = counts.rejected, cap = crate::plugins::inbox::MAX_QUEUED_EVENTS_PER_PLUGIN, "plugin event queue saturated");
            } else if counts.coalesced > 0 {
                tracing::debug!(target: "thegn::plugin", plugin = %plugin, coalesced = counts.coalesced, "plugin event queue coalesced updates");
            }
        }
    }
    let alerts = std::mem::take(&mut state.pending_alerts);
    for (plugin, dropped) in std::mem::take(&mut state.pending_alert_drops) {
        tracing::warn!(target: "thegn::plugin", plugin = %plugin, dropped, cap = MAX_PENDING_ALERTS_PER_PLUGIN, "plugin alerts dropped: per-drain outbox cap reached");
    }
    if alerts.is_empty() {
        return;
    }
    submit(alerts);
}

/// Apply up to [`MAX_PLUGIN_MESSAGES_PER_DRAIN`] queued message units, preserving
/// FIFO order and leaving any remaining messages queued for the caller to
/// re-poll. A one-shot run is applied whole even if it exceeds the budget.
/// Returns the narrowest aggregate render damage (statusbar content, structural
/// lifecycle/contribution change, or none). The before/after comparison is
/// deliberately made around the whole drain so coalesced updates that return
/// to their original view stay idle.
/// `host` is the respawn/shutdown handle; `None` (tests) records restart
/// decisions without spawning anything.
pub(crate) fn drain(
    rx: &mut PluginRx,
    state: &mut PluginsState,
    model: &mut FrameModel,
    host: Option<&PluginsHost>,
) -> PluginDamage {
    drain_with_responder(rx, state, model, host, &mut respond)
}

fn drain_with_responder(
    rx: &mut PluginRx,
    state: &mut PluginsState,
    model: &mut FrameModel,
    host: Option<&PluginsHost>,
    responder: &mut impl FnMut(&PluginEntry, RpcResponse),
) -> PluginDamage {
    state.inbox = Some(rx.inbox());
    let before_views = statusbar_views(state);
    let mut damage = PluginDamage::None;
    let mut applied = 0;
    while applied < MAX_PLUGIN_MESSAGES_PER_DRAIN {
        if let Some(rejected) = rx.pop_rejected_if_lifecycle_empty() {
            applied = applied.saturating_add(1);
            answer_rejected(state, rejected, responder);
            continue;
        }
        let Ok(msg) = rx.try_recv() else {
            break;
        };
        applied = applied.saturating_add(match &msg {
            PluginMsg::OneShot { run, .. } => match run {
                Ok(run) => run.messages.len().max(1),
                Err(_) => 1,
            },
            PluginMsg::Event { .. }
            | PluginMsg::Loaded(_)
            | PluginMsg::Refused(_)
            | PluginMsg::Respawned { .. }
            | PluginMsg::RespawnWorkerStartFailed { .. }
            | PluginMsg::RetryRespawn { .. } => 1,
        });
        match msg {
            PluginMsg::Loaded(list) => {
                for st in list {
                    let id = st.plugin.spec.manifest.id.as_str().to_string();
                    let entry = build_entry(st);
                    send_activate(&entry);
                    let failed_resident_start =
                        entry.plugin.spec.mode == PluginMode::Resident && entry.writer.is_none();
                    state.plugins.insert(id.clone(), entry);
                    if failed_resident_start {
                        damage.merge(handle_exit(state, model, host, &id, None, false));
                    }
                }
                sync_provider_registry(state);
                damage.merge(PluginDamage::Structural);
            }
            PluginMsg::Refused(problems) => {
                if let Some(first) = problems.first() {
                    model.status = if problems.len() > 1 {
                        format!(
                            "{} (+{} more; see thegn plugin check)",
                            first,
                            problems.len() - 1
                        )
                    } else {
                        first.to_string()
                    };
                }
                damage.merge(PluginDamage::Structural);
            }
            PluginMsg::Event { plugin, event } => match event {
                SessionEvent::Message(m) => damage.merge(apply_message(state, &plugin, m)),
                SessionEvent::Response(r) => {
                    // Provider replies are resolved by the owning session before
                    // enqueueing host events. Never route an old session's id
                    // against a replacement bridge with the same plugin name.
                    tracing::debug!(target: "thegn::plugin", plugin = %plugin, id = r.id, "unroutable response from originating plugin session");
                }
                SessionEvent::Junk(line) => {
                    tracing::debug!(target: "thegn::plugin", plugin = %plugin, "junk: {line}");
                }
                SessionEvent::Exit { code } => {
                    damage.merge(handle_exit(state, model, host, &plugin, code, false));
                }
            },
            PluginMsg::OneShot { plugin, run } => match run {
                Ok(run) => {
                    for line in &run.junk {
                        tracing::debug!(target: "thegn::plugin", plugin = %plugin, "junk: {line}");
                    }
                    if run.truncated {
                        tracing::warn!(target: "thegn::plugin", plugin = %plugin, "one-shot output truncated");
                    }
                    for m in run.messages {
                        damage.merge(apply_message(state, &plugin, m));
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "thegn::plugin", plugin = %plugin, error = %e, "one-shot run failed");
                }
            },
            PluginMsg::Respawned { plugin, writer } => {
                if !state.plugins.contains_key(&plugin) {
                    continue;
                }
                match writer {
                    Some(w) => {
                        let entry = state.plugins.get_mut(&plugin).expect("checked above");
                        entry.runtime.begin_generation();
                        entry.feed_bridge = None;
                        if entry
                            .contributions
                            .iter()
                            .any(|c| c.extension_point == ExtensionPoint::IssueProvider)
                        {
                            entry.bridge = Some(thegn_svc::plugin::ProviderBridge::new(
                                w.clone(),
                                entry.plugin.spec.run_timeout(),
                            ));
                        }
                        entry.writer = Some(w);
                        send_activate(entry);
                        sync_provider_registry(state);
                        damage.merge(PluginDamage::Structural);
                    }
                    // The respawn itself failed: treat like another crash so
                    // the backoff keeps climbing toward the cap.
                    None => damage.merge(handle_exit(state, model, host, &plugin, None, false)),
                }
            }
            PluginMsg::RespawnWorkerStartFailed { plugin } => {
                damage.merge(handle_exit(state, model, host, &plugin, None, true));
            }
            PluginMsg::RetryRespawn { plugin } => {
                if let Some(host) = host {
                    host.respawn(plugin, std::time::Duration::ZERO);
                }
            }
        }
    }
    if damage < PluginDamage::Structural {
        damage.merge(crate::plugin_damage::classify(
            &before_views,
            &statusbar_views(state),
        ));
    }
    damage
}

fn answer_rejected(
    state: &PluginsState,
    rejected: RejectedRequest,
    responder: &mut impl FnMut(&PluginEntry, RpcResponse),
) {
    if let Some(entry) = state.plugins.get(&rejected.plugin) {
        responder(
            entry,
            RpcResponse::err(
                rejected.id,
                RpcError::new(RpcErrorCode::RateLimited, "host plugin queue full"),
            ),
        );
    } else {
        tracing::debug!(target: "thegn::plugin", plugin = %rejected.plugin, id = rejected.id, "rejected request has no live plugin entry");
    }
}

/// A resident session died: decide restart (backoff, capped at 3) or mark the
/// plugin disabled-until-reload.
fn handle_exit(
    state: &mut PluginsState,
    model: &mut FrameModel,
    host: Option<&PluginsHost>,
    plugin: &str,
    code: Option<i32>,
    worker_start_failed: bool,
) -> PluginDamage {
    let Some(entry) = state.plugins.get_mut(plugin) else {
        return PluginDamage::None;
    };
    entry.writer = None;
    entry.feed_bridge = None;
    entry.bridge = None;
    sync_provider_registry(state);
    let entry = state.plugins.get_mut(plugin).expect("looked up above");
    if entry.disabled {
        return PluginDamage::None;
    }
    match crate::plugins::restart_delay(entry.restarts) {
        Some(delay) => {
            entry.restarts += 1;
            tracing::warn!(
                target: "thegn::plugin",
                plugin = %plugin, code = ?code, restart = entry.restarts, delay_s = delay.as_secs(),
                "plugin exited; restarting"
            );
            if let Some(host) = host {
                if worker_start_failed {
                    host.retry_respawn_after_worker_start_failure(entry.plugin.clone(), delay);
                } else {
                    host.respawn(entry.plugin.clone(), delay);
                }
            }
        }
        None => {
            entry.disabled = true;
            if let Some(host) = host {
                host.set_disabled(plugin);
            }
            model.status = format!("plugin {plugin} keeps crashing — disabled until config reload");
        }
    }
    PluginDamage::Structural
}

/// Map a runtime error onto the wire vocabulary.
fn rpc_error_of(e: PluginApiError) -> RpcError {
    let code = match &e {
        PluginApiError::CapabilityDenied { .. } => RpcErrorCode::Denied,
        PluginApiError::UnsupportedExtensionPoint(_) | PluginApiError::UnknownExtensionPoint(_) => {
            RpcErrorCode::Invalid
        }
        PluginApiError::UndeclaredContribution(_)
        | PluginApiError::ContributionTypeMismatch { .. }
        | PluginApiError::ContributionChanged(_)
        | PluginApiError::QuotaExceeded { .. } => RpcErrorCode::Invalid,
        PluginApiError::UnsupportedIo(_) => RpcErrorCode::Unsupported,
        PluginApiError::IncompatibleApi { .. } => RpcErrorCode::Other,
    };
    RpcError::new(code, e.to_string())
}

/// Answer an id-bearing request through the entry's writer (one-shot plugins
/// have none; their replies are dropped with a debug note).
fn respond(entry: &PluginEntry, resp: RpcResponse) {
    match &entry.writer {
        // best-effort: a session mid-exit errors; the Exit event cleans up.
        Some(w) => {
            let _ = w.respond(&resp); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        }
        None => tracing::debug!(
            target: "thegn::plugin",
            plugin = %entry.plugin.spec.manifest.id.as_str(), id = resp.id,
            "no session writer; response dropped"
        ),
    }
}

fn param_str(params: &serde_json::Value, key: &str) -> Option<String> {
    params.get(key)?.as_str().map(str::to_string)
}

fn parse_register_contribution(params: serde_json::Value) -> Result<Contribution, RpcError> {
    serde_json::from_value::<Contribution>(params)
        .map_err(|e| RpcError::new(RpcErrorCode::Invalid, format!("bad contribution: {e}")))
}

/// Apply one verb from a plugin. Lifecycle/contribution changes are structural;
/// view updates are classified once the entire drain has been coalesced.
fn apply_message(state: &mut PluginsState, plugin: &str, msg: RpcMessage) -> PluginDamage {
    let Some(verb) = HostVerb::ALL
        .iter()
        .copied()
        .find(|v| v.method_name() == msg.method)
    else {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, method = %msg.method, "unknown method");
        if let (Some(id), Some(entry)) = (msg.id, state.plugins.get(plugin)) {
            respond(
                entry,
                RpcResponse::err(
                    id,
                    RpcError::new(
                        RpcErrorCode::Invalid,
                        format!("unknown method {}", msg.method),
                    ),
                ),
            );
        }
        return PluginDamage::None;
    };
    let Some(entry) = state.plugins.get(plugin) else {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, "message from unknown plugin");
        return PluginDamage::None;
    };
    if let Err(error) = host_verb_check(&entry.plugin.spec, verb) {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, verb = verb.method_name(), error = %error.message, "verb rejected by support contract");
        if let Some(id) = msg.id {
            respond(entry, RpcResponse::err(id, error));
        }
        return PluginDamage::None;
    }
    // host.call needs `&mut state` for the lazy dispatcher, so it is handled
    // before the per-entry borrow below.
    if verb == HostVerb::HostCall {
        return apply_host_call(state, plugin, msg);
    }
    let entry = state
        .plugins
        .get_mut(plugin)
        .expect("plugin existence checked before verb dispatch");
    let pid = PluginId::new(plugin);
    let params = msg.params;
    // The verb's effect, plus the value (if any) an id-bearing request gets
    // back. Invalid params short-circuit to an Invalid error response.
    let damage = PluginDamage::None;
    let outcome: Result<serde_json::Value, RpcError> = match verb {
        HostVerb::Register => parse_register_contribution(params).and_then(|c| {
            if let Err(error) = entry.runtime.register(pid.clone(), c.clone()) {
                tracing::debug!(
                    target: "thegn::plugin",
                    plugin = %plugin,
                    contribution = %c.id.as_str(),
                    error = %error,
                    "plugin contribution registration rejected"
                );
                return Err(rpc_error_of(error));
            }
            Ok(serde_json::Value::Null)
        }),
        HostVerb::Update => {
            let surface = param_str(&params, "surface");
            let view = params
                .get("view")
                .cloned()
                .map(serde_json::from_value::<View>);
            match (surface, view) {
                (Some(s), Some(Ok(v))) => entry
                    .runtime
                    .update(pid.clone(), SurfaceId::new(s), v)
                    .map_err(rpc_error_of)
                    .map(|_| serde_json::Value::Null),
                _ => Err(RpcError::new(
                    RpcErrorCode::Invalid,
                    "update params must be {\"surface\": \"…\", \"view\": {…}}",
                )),
            }
        }
        HostVerb::Invalidate => match param_str(&params, "surface") {
            Some(s) => entry
                .runtime
                .invalidate(pid.clone(), SurfaceId::new(s))
                .map_err(rpc_error_of)
                .map(|()| serde_json::Value::Null),
            None => Err(RpcError::new(RpcErrorCode::Invalid, "missing surface")),
        },
        HostVerb::Subscribe => {
            let kinds: Result<Vec<EventKind>, _> = params
                .get("events")
                .cloned()
                .map(serde_json::from_value)
                .unwrap_or_else(|| Ok(Vec::new()));
            match kinds {
                Ok(kinds) => subscribe_kinds(&mut entry.runtime, pid, kinds)
                    .map(|()| serde_json::Value::Null),
                Err(e) => Err(RpcError::new(
                    RpcErrorCode::Invalid,
                    format!("bad events list: {e}"),
                )),
            }
        }
        HostVerb::Emit => serde_json::from_value::<Event>(params)
            .map_err(|e| RpcError::new(RpcErrorCode::Invalid, format!("bad event: {e}")))
            .and_then(|ev| {
                entry
                    .runtime
                    .emit(pid.clone(), ev)
                    .map_err(rpc_error_of)
                    .map(|()| serde_json::Value::Null)
            }),
        HostVerb::Io => serde_json::from_value::<IoRequest>(params)
            .map_err(|e| RpcError::new(RpcErrorCode::Invalid, format!("bad io request: {e}")))
            .and_then(|req| {
                entry
                    .runtime
                    .io(pid.clone(), req)
                    .map_err(rpc_error_of)
                    .and_then(|res| {
                        serde_json::to_value(res)
                            .map_err(|e| RpcError::new(RpcErrorCode::Other, e.to_string()))
                    })
            }),
        HostVerb::Notify => serde_json::from_value::<Alert>(params)
            .map_err(|e| RpcError::new(RpcErrorCode::Invalid, format!("bad alert: {e}")))
            .and_then(|mut alert| {
                if alert.message.len() > MAX_ALERT_MESSAGE_BYTES {
                    let mut end = MAX_ALERT_MESSAGE_BYTES;
                    while !alert.message.is_char_boundary(end) {
                        end -= 1;
                    }
                    alert.message.truncate(end);
                }
                entry
                    .runtime
                    .notify(pid.clone(), alert.clone())
                    .map_err(rpc_error_of)?;
                // Route through the same inbox other producers use; the DB
                // write happens off-loop in `flush_alerts`.
                let queued = state
                    .pending_alerts
                    .iter()
                    .filter(|(queued_plugin, _)| queued_plugin == plugin)
                    .count();
                if queued < MAX_PENDING_ALERTS_PER_PLUGIN {
                    state.pending_alerts.push((plugin.to_string(), alert));
                } else {
                    *state
                        .pending_alert_drops
                        .entry(plugin.to_string())
                        .or_default() += 1;
                }
                Ok(serde_json::Value::Null)
            }),
        HostVerb::StateSet => {
            let key = param_str(&params, "key");
            let val = params
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match key {
                Some(k) => entry
                    .runtime
                    .state_set(pid.clone(), &k, val)
                    .map_err(rpc_error_of)
                    .map(|()| serde_json::Value::Null),
                None => Err(RpcError::new(RpcErrorCode::Invalid, "missing key")),
            }
        }
        HostVerb::StateGet => match param_str(&params, "key") {
            Some(k) => entry
                .runtime
                .state_get(pid.clone(), &k)
                .map_err(rpc_error_of)
                .map(|v| v.unwrap_or(serde_json::Value::Null)),
            None => Err(RpcError::new(RpcErrorCode::Invalid, "missing key")),
        },
        HostVerb::HostValue => match param_str(&params, "key") {
            Some(k) => entry
                .runtime
                .host_value(pid.clone(), &k)
                .map_err(rpc_error_of)
                .map(|v| v.unwrap_or(serde_json::Value::Null)),
            None => Err(RpcError::new(RpcErrorCode::Invalid, "missing key")),
        },
        HostVerb::HostCall => unreachable!("handled above"),
    };
    if let Some(id) = msg.id {
        let resp = match outcome {
            Ok(v) => RpcResponse::ok(id, v),
            Err(e) => RpcResponse::err(id, e),
        };
        respond(entry, resp);
    } else if let Err(e) = outcome {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, verb = verb.method_name(), error = %e.message, "verb failed");
    }
    damage
}

fn subscribe_kinds(
    runtime: &mut PluginRuntime,
    plugin: PluginId,
    kinds: Vec<EventKind>,
) -> Result<(), RpcError> {
    if kinds.len() > MAX_SUBSCRIPTIONS_PER_PLUGIN {
        return Err(RpcError::new(
            RpcErrorCode::RateLimited,
            format!("subscription list exceeds {MAX_SUBSCRIPTIONS_PER_PLUGIN} events"),
        ));
    }
    for kind in kinds {
        if !runtime
            .subscriptions()
            .contains(&(plugin.clone(), kind.clone()))
            && runtime.subscriptions().len() >= MAX_SUBSCRIPTIONS_PER_PLUGIN
        {
            return Err(RpcError::new(
                RpcErrorCode::RateLimited,
                format!("plugin subscription limit is {MAX_SUBSCRIPTIONS_PER_PLUGIN}"),
            ));
        }
        runtime
            .subscribe(plugin.clone(), kind)
            .map_err(rpc_error_of)?;
    }
    Ok(())
}

/// `host.call`: scope-check on the loop, dispatch on the dispatcher thread,
/// answer directly through the plugin's writer.
fn apply_host_call(state: &mut PluginsState, plugin: &str, msg: RpcMessage) -> PluginDamage {
    let feed_admission = (param_str(&msg.params, "cap").as_deref() == Some("events.subscribe"))
        .then(|| feed_bridge_admission(state, plugin));
    let Some(entry) = state.plugins.get_mut(plugin) else {
        return PluginDamage::None;
    };
    let Some(id) = msg.id else {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, "host.call without id ignored");
        return PluginDamage::None;
    };
    let Some(writer) = entry.writer.clone() else {
        // One-shot plugins cannot receive replies — skip dispatch entirely.
        tracing::warn!(target: "thegn::plugin", plugin = %plugin, "host.call from a one-shot plugin skipped (no reply channel)");
        return PluginDamage::None;
    };
    let Some(cap) = param_str(&msg.params, "cap") else {
        respond(
            entry,
            RpcResponse::err(id, RpcError::new(RpcErrorCode::Invalid, "missing cap")),
        );
        return PluginDamage::None;
    };
    let params = msg
        .params
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if let Err(e) = authorize_host_call(entry, &cap, &params) {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, cap = %cap, error = %e.message, "host.call denied");
        respond(entry, RpcResponse::err(id, e));
        return PluginDamage::None;
    }
    // The event feed is a stream, not a request/response: `events.subscribe`
    // starts an off-loop bridge that forwards feed frames as `on_event`
    // notifications, and the host.call itself is acknowledged immediately.
    if cap == "events.subscribe" {
        match feed_admission.expect("events.subscribe admission was computed") {
            Ok(false) => respond(
                entry,
                RpcResponse::ok(id, serde_json::json!({ "subscribed": true })),
            ),
            Err(error) => respond(entry, RpcResponse::err(id, error)),
            Ok(true) => {
                let bridge = if claim_feed_bridge(entry) {
                    spawn_feed_bridge(Arc::clone(&state.cfg), writer.clone())
                } else {
                    None
                };
                match bridge {
                    Some(bridge) => {
                        entry.feed_bridge = Some(bridge);
                        respond(
                            entry,
                            RpcResponse::ok(id, serde_json::json!({ "subscribed": true })),
                        );
                    }
                    None => respond(
                        entry,
                        RpcResponse::err(
                            id,
                            RpcError::new(RpcErrorCode::Other, "feed bridge could not be started"),
                        ),
                    ),
                }
            }
        }
        return PluginDamage::None;
    }
    let dispatcher = state.dispatcher();
    dispatcher.dispatch(plugin, writer, id, cap, params);
    PluginDamage::None
}

/// Claim the per-session feed bridge slot, reclaiming a completed bridge.
fn claim_feed_bridge(entry: &mut PluginEntry) -> bool {
    if let Some(bridge) = &entry.feed_bridge
        && !bridge.handle.is_finished()
    {
        return false;
    }
    entry.feed_bridge = None;
    true
}

/// Count live bridges before taking a mutable plugin-entry borrow. A second
/// request for the same live entry is an idempotent acknowledgment.
fn feed_bridge_admission(state: &PluginsState, plugin: &str) -> Result<bool, RpcError> {
    if state.plugins.get(plugin).is_some_and(|entry| {
        entry
            .feed_bridge
            .as_ref()
            .is_some_and(|bridge| !bridge.handle.is_finished())
    }) {
        return Ok(false);
    }
    let live = state
        .plugins
        .values()
        .filter(|entry| {
            entry
                .feed_bridge
                .as_ref()
                .is_some_and(|bridge| !bridge.handle.is_finished())
        })
        .count();
    if live >= MAX_FEED_BRIDGES {
        return Err(RpcError::new(
            RpcErrorCode::RateLimited,
            format!("feed bridge limit is {MAX_FEED_BRIDGES}"),
        ));
    }
    Ok(true)
}

/// Start an off-loop bridge that forwards the daemon control event feed to a
/// resident plugin as `on_event` notifications (the plugin declared a feed
/// subscription via `host.call events.subscribe`, read-scoped). A dedicated
/// thread owns a current-thread runtime + the feed subscription and writes
/// straight to the plugin's [`SessionWriter`] — it never re-enters the
/// compositor loop, so a subscribing plugin costs the idle render loop nothing.
/// The bridge ends when the feed closes or the plugin's session goes away.
fn spawn_cancellable_bridge(
    name: &'static str,
    fut: impl std::future::Future<Output = ()> + Send + 'static,
) -> Option<FeedBridge> {
    debug_assert!(name.len() <= 15);
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let spawn = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(target: "thegn::plugin", error = %e, "feed-bridge runtime failed");
                    return;
                }
            };
            rt.block_on(async move {
                tokio::select! {
                    _ = cancel_rx => {},
                    _ = fut => {},
                }
            });
        });
    match spawn {
        Ok(handle) => Some(FeedBridge {
            handle: Arc::new(handle),
            _cancel: cancel_tx,
        }),
        Err(e) => {
            tracing::warn!(target: "thegn::plugin", error = %e, "feed-bridge thread failed to start");
            None
        }
    }
}

fn spawn_feed_bridge(
    cfg: Arc<thegn_core::config::Config>,
    writer: SessionWriter,
) -> Option<FeedBridge> {
    spawn_feed_bridge_worker(async move {
        let client = match crate::cmd::session::connect(&cfg).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(target: "thegn::plugin", error = %e, "feed-bridge: no daemon");
                return;
            }
        };
        let mut stream = match client.subscribe_events().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(target: "thegn::plugin", error = %e, "feed-bridge subscribe failed");
                return;
            }
        };
        while let Some(frame) = stream.frames.recv().await {
            use thegn_core::control_wire::EventFrame;
            // Pane byte streams never reach a plugin this way; and the
            // greeting frame is not a feed event.
            if matches!(
                frame,
                EventFrame::Hello(_)
                    | EventFrame::PaneSnapshot { .. }
                    | EventFrame::PaneDelta { .. }
            ) {
                continue;
            }
            let payload = thegn_svc::control::http::frame_json(&frame);
            if writer
                .notify(
                    thegn_core::plugin_api::PluginCallback::OnEvent,
                    serde_json::json!({
                        "kind": thegn_core::plugin_api::EventKind::Custom(
                            "control.feed".into()
                        ),
                        "payload": payload,
                    }),
                )
                .is_err()
            {
                // The plugin's session is gone — stop forwarding.
                return;
            }
        }
    })
}

fn spawn_feed_bridge_worker(
    fut: impl std::future::Future<Output = ()> + Send + 'static,
) -> Option<FeedBridge> {
    spawn_cancellable_bridge(FEED_BRIDGE_THREAD, fut)
}

// ---------------------------------------------------------------------------
// host.call dispatcher
// ---------------------------------------------------------------------------

struct DispatchReq {
    plugin: String,
    reply: DispatchReply,
    id: u64,
    cap: String,
    /// Parameters for the capability (path placeholders + body/query), resolved
    /// against the `API_CALLS` route spine.
    params: serde_json::Value,
    _permit: DispatchPermit,
}

type DispatchRespond = dyn Fn(&RpcResponse) -> Result<(), SessionFailure> + Send;
type DispatchIsClosed = dyn Fn() -> bool + Send;

struct DispatchReply {
    respond: Box<DispatchRespond>,
    is_closed: Box<DispatchIsClosed>,
}

impl DispatchReply {
    fn writer(writer: SessionWriter) -> Self {
        let respond_writer = writer.clone();
        Self {
            respond: Box::new(move |response| respond_writer.respond(response)),
            is_closed: Box::new(move || writer.is_closed()),
        }
    }

    fn respond(&self, response: &RpcResponse) -> Result<(), SessionFailure> {
        (self.respond)(response)
    }

    fn is_closed(&self) -> bool {
        (self.is_closed)()
    }
}

struct DispatchPermit {
    plugin: String,
    load: Arc<std::sync::Mutex<DispatchLoad>>,
}

impl Drop for DispatchPermit {
    fn drop(&mut self) {
        let mut load = self.load.lock().unwrap_or_else(|error| error.into_inner());
        load.total = load.total.saturating_sub(1);
        if let Some(count) = load.per_plugin.get_mut(&self.plugin) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                load.per_plugin.remove(&self.plugin);
            }
        }
    }
}

fn acquire_dispatch_permit(
    load: &Arc<std::sync::Mutex<DispatchLoad>>,
    plugin: &str,
) -> Option<DispatchPermit> {
    let mut load_guard = load.lock().unwrap_or_else(|error| error.into_inner());
    let plugin_count = load_guard
        .per_plugin
        .get(plugin)
        .copied()
        .unwrap_or_default();
    if load_guard.total >= DISPATCH_QUEUE_CAPACITY
        || plugin_count >= MAX_DISPATCH_IN_FLIGHT_PER_PLUGIN
    {
        return None;
    }
    load_guard.total += 1;
    *load_guard.per_plugin.entry(plugin.to_owned()).or_default() += 1;
    Some(DispatchPermit {
        plugin: plugin.to_owned(),
        load: Arc::clone(load),
    })
}

#[derive(Default)]
struct DispatchLoad {
    total: usize,
    per_plugin: BTreeMap<String, usize>,
}

#[derive(Default)]
struct DispatchDiagnostics {
    rejected: usize,
    timed_out: usize,
    unavailable: usize,
    abandoned: usize,
    undelivered: usize,
}

#[derive(Default)]
struct DispatchDiagnosticsByPlugin(BTreeMap<String, DispatchDiagnostics>);

impl DispatchDiagnosticsByPlugin {
    fn record(&mut self, plugin: &str, update: impl FnOnce(&mut DispatchDiagnostics)) {
        update(self.0.entry(plugin.to_owned()).or_default());
    }

    fn take(&mut self) -> BTreeMap<String, DispatchDiagnostics> {
        std::mem::take(&mut self.0)
    }
}

/// One background thread owning a current-thread tokio runtime + the control
/// client, answering plugins directly through their [`SessionWriter`]s (never
/// re-entering the event loop). A worker panic can still drop already-queued
/// requests; subsequent requests observe a disconnected channel and get `Other`.
pub(crate) struct Dispatcher {
    tx: std::sync::mpsc::SyncSender<DispatchReq>,
    load: Arc<std::sync::Mutex<DispatchLoad>>,
    diagnostics: Arc<std::sync::Mutex<DispatchDiagnosticsByPlugin>>,
}

impl Dispatcher {
    fn spawn(cfg: Arc<thegn_core::config::Config>) -> Self {
        Self::spawn_with(DISPATCH_DEADLINE, move |cap, params| {
            let cfg = Arc::clone(&cfg);
            Box::pin(async move { dispatch_one(&cfg, &cap, params).await })
        })
    }

    fn spawn_with(
        deadline: std::time::Duration,
        dispatch: impl Fn(String, serde_json::Value) -> DispatchFuture + Send + 'static,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<DispatchReq>(DISPATCH_QUEUE_CAPACITY);
        let load = Arc::new(std::sync::Mutex::new(DispatchLoad::default()));
        let diagnostics = Arc::new(std::sync::Mutex::new(DispatchDiagnosticsByPlugin::default()));
        let worker_diagnostics = Arc::clone(&diagnostics);
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(error) => {
                tracing::warn!(target: "thegn::plugin", error = %error, "dispatcher runtime failed");
                return Self {
                    tx,
                    load,
                    diagnostics,
                };
            }
        };
        let _thread = thegn_core::thread_name::spawn_named("thegn-plugdisp", move || {
            // Utility: plugin host.call round-trips back visible plugin content.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            while let Ok(req) = rx.recv() {
                let DispatchReq {
                    plugin,
                    reply,
                    id,
                    cap,
                    params,
                    _permit,
                } = req;
                if reply.is_closed() {
                    worker_diagnostics
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .record(&plugin, |counts| counts.abandoned += 1);
                    drop(_permit);
                    continue;
                }
                // Timeout cancels this local future. Synchronous setup before its first
                // await may already have run, and a daemon mutation may have completed.
                let response = match rt
                    .block_on(async { tokio::time::timeout(deadline, dispatch(cap, params)).await })
                {
                    Ok(Ok(value)) => RpcResponse::ok(id, value),
                    Ok(Err(error)) => RpcResponse::err(id, error),
                    Err(_) => {
                        worker_diagnostics
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .record(&plugin, |counts| counts.timed_out += 1);
                        RpcResponse::err(
                            id,
                            RpcError::new(RpcErrorCode::Timeout, "host.call timed out"),
                        )
                    }
                };
                drop(_permit);
                // best-effort: the session may have died mid-dispatch.
                if let Err(error) = reply.respond(&response) {
                    let mut diagnostics = worker_diagnostics
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    match error {
                        SessionFailure::Full | SessionFailure::TooLarge => {
                            diagnostics.record(&plugin, |counts| counts.undelivered += 1);
                        }
                        SessionFailure::Closed => {
                            diagnostics.record(&plugin, |counts| counts.abandoned += 1);
                        }
                    }
                    tracing::debug!(target: "thegn::plugin", error = %error, "host.call response writer is gone");
                }
            }
        });
        Self {
            tx,
            load,
            diagnostics,
        }
    }

    fn dispatch(
        &self,
        plugin: &str,
        writer: SessionWriter,
        id: u64,
        cap: String,
        params: serde_json::Value,
    ) {
        self.dispatch_with_reply(plugin, DispatchReply::writer(writer), id, cap, params);
    }

    fn dispatch_with_reply(
        &self,
        plugin: &str,
        reply: DispatchReply,
        id: u64,
        cap: String,
        params: serde_json::Value,
    ) {
        let Some(permit) = acquire_dispatch_permit(&self.load, plugin) else {
            self.record(plugin, |counts| counts.rejected += 1);
            self.answer_error(
                plugin,
                &reply,
                id,
                RpcErrorCode::RateLimited,
                "host.call dispatcher is full",
            );
            return;
        };
        let req = DispatchReq {
            plugin: plugin.to_owned(),
            reply,
            id,
            cap,
            params,
            _permit: permit,
        };
        match self.tx.try_send(req) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(req)) => {
                self.record(&req.plugin, |counts| counts.rejected += 1);
                self.answer_error(
                    &req.plugin,
                    &req.reply,
                    req.id,
                    RpcErrorCode::RateLimited,
                    "host.call dispatcher is full",
                );
                drop(req);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(req)) => {
                self.record(&req.plugin, |counts| counts.unavailable += 1);
                self.answer_error(
                    &req.plugin,
                    &req.reply,
                    req.id,
                    RpcErrorCode::Other,
                    "host.call dispatcher stopped",
                );
                drop(req);
            }
        }
    }

    fn record(&self, plugin: &str, update: impl FnOnce(&mut DispatchDiagnostics)) {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .record(plugin, update);
    }

    fn answer_error(
        &self,
        plugin: &str,
        reply: &DispatchReply,
        id: u64,
        code: RpcErrorCode,
        message: &str,
    ) {
        if let Err(error) = reply.respond(&RpcResponse::err(id, RpcError::new(code, message))) {
            match error {
                SessionFailure::Full | SessionFailure::TooLarge => {
                    self.record(plugin, |counts| counts.undelivered += 1);
                }
                SessionFailure::Closed => {
                    self.record(plugin, |counts| counts.abandoned += 1);
                }
            }
            tracing::debug!(target: "thegn::plugin", error = %error, "host.call rejection response writer is gone");
        }
    }

    fn take_diagnostics(&self) -> BTreeMap<String, DispatchDiagnostics> {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    #[cfg(test)]
    fn disconnected() -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<DispatchReq>(DISPATCH_QUEUE_CAPACITY);
        drop(rx);
        Self {
            tx,
            load: Arc::new(std::sync::Mutex::new(DispatchLoad::default())),
            diagnostics: Arc::new(std::sync::Mutex::new(DispatchDiagnosticsByPlugin::default())),
        }
    }
}

type DispatchFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<serde_json::Value, RpcError>> + Send>,
>;

/// Run one dispatched capability against the pane daemon's control API,
/// generically: resolve `(method, path, body)` from the catalog + `API_CALLS`
/// route spine (the same one `thegn api call` uses) and perform it over the
/// control socket, reusing the CLI's discovery + client
/// (`cmd::session::connect`). No per-verb code — a newly routed catalog verb
/// listing `Surface::Plugin` is callable the moment its route lands.
async fn dispatch_one(
    cfg: &thegn_core::config::Config,
    cap: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let (method, path, body) = crate::cmd::api::resolve_call(cap, params)
        .map_err(|e| RpcError::new(RpcErrorCode::Invalid, e.to_string()))?;
    let client = crate::cmd::session::connect(cfg)
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::Other, e.to_string()))?;
    client
        .call_raw(method, &path, body)
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::Other, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::control::Scope;
    use thegn_core::plugin_api::{
        ApiVersion, CadenceHint, ContributionId, PluginManifest, Span, StyleRole,
    };

    #[test]
    fn alert_flush_submits_one_batch_per_drain_without_a_runtime() {
        let mut state = state_with(Vec::new());
        state.pending_alerts.extend([
            ("p".into(), Alert::new("source-a", "one")),
            ("q".into(), Alert::new("source-b", "two")),
            ("p".into(), Alert::new("source-c", "three")),
        ]);
        let mut captured = Vec::new();

        flush_alerts_with(&mut state, |batch| captured.push(batch));

        assert_eq!(captured.len(), 1, "one non-empty drain submits one job");
        let messages: Vec<_> = captured[0]
            .iter()
            .map(|(plugin, alert)| (plugin.as_str(), alert.message.as_str()))
            .collect();
        assert_eq!(messages, [("p", "one"), ("q", "two"), ("p", "three")]);
        assert!(state.pending_alerts.is_empty());
    }

    #[test]
    fn alert_flush_submits_nothing_without_alerts() {
        let mut state = state_with(Vec::new());
        let mut submitted = false;

        flush_alerts_with(&mut state, |_| submitted = true);

        assert!(!submitted, "an empty drain submits no writer job");
    }

    #[test]
    fn alert_flush_batch_writes_one_notification_per_alert() {
        use thegn_core::store::NotificationStore;

        let db = thegn_core::db::Db::open_memory().unwrap();
        persist_alert_batch(
            &db,
            vec![
                ("p".into(), Alert::new("p", "one")),
                ("q".into(), Alert::new("q", "two")),
            ],
        );

        let notifications = db.get_all_notifications(10).unwrap();
        let messages: Vec<_> = notifications
            .iter()
            .map(|notification| notification.message.as_str())
            .collect();
        assert_eq!(messages.len(), 2, "each alert writes one notification");
        assert!(messages.contains(&"p: one"));
        assert!(messages.contains(&"q: two"));
    }

    fn test_reply() -> (DispatchReply, std::sync::mpsc::Receiver<RpcResponse>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            DispatchReply {
                respond: Box::new(move |response| {
                    tx.send(response.clone())
                        .map_err(|_| SessionFailure::Closed)
                }),
                is_closed: Box::new(|| false),
            },
            rx,
        )
    }

    fn never_dispatch(_: String, _: serde_json::Value) -> DispatchFuture {
        Box::pin(std::future::pending())
    }

    #[test]
    fn dispatcher_reuses_shared_config_after_first_host_call() {
        let mut state = PluginsState::new(thegn_core::config::Config::default());

        assert_eq!(Arc::strong_count(&state.cfg), 1);
        state.dispatcher();
        assert_eq!(Arc::strong_count(&state.cfg), 2);
        state.dispatcher();
        assert_eq!(Arc::strong_count(&state.cfg), 2);
    }

    #[test]
    fn apply_host_call_reuses_shared_config_for_multiple_calls() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let supervisor = thegn_svc::plugin::ResidentSupervisor::new(runtime.handle().clone());
        let executable = std::env::current_exe().expect("test executable");
        let args = vec![
            "--exact".to_owned(),
            "handlers::plugins::tests::resident_fixture_child".to_owned(),
        ];
        let mut env = BTreeMap::new();
        env.insert("THEGN_PLUGIN_TEST_HOLD".to_owned(), "1".to_owned());
        let session = thegn_svc::plugin::ResidentSession::spawn(
            &supervisor,
            "apply-host-call-config-test",
            &std::iter::once(executable.to_string_lossy().into_owned())
                .chain(args)
                .collect::<Vec<_>>(),
            &env,
            None,
            |_| {},
        )
        .expect("resident fixture starts");

        let mut state = state_with(vec![spec("p", Vec::new(), vec![Scope::Read])]);
        state.plugins.get_mut("p").expect("plugin entry").writer = Some(session.writer());

        for id in [1, 2] {
            assert!(matches!(
                apply_host_call(
                    &mut state,
                    "p",
                    RpcMessage {
                        id: Some(id),
                        method: "host.call".into(),
                        // The missing route placeholder makes dispatch fail before
                        // connecting to a daemon, while still exercising admission.
                        params: serde_json::json!({ "cap": "sessions.snapshot", "params": null }),
                    },
                ),
                PluginDamage::None
            ));
        }
        assert!(state.dispatcher.is_some());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let total = state
                .dispatcher
                .as_ref()
                .expect("host.call starts dispatcher")
                .load
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .total;
            if total == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "dispatcher requests did not finish before shared-config assertion"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(Arc::strong_count(&state.cfg), 2);

        session.kill();
        let mut completion = session.completion();
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while completion.borrow().is_none() {
                    completion.changed().await.expect("session completion");
                }
            })
            .await
            .expect("resident fixture terminates");
        });
    }

    #[test]
    fn resident_fixture_child() {
        if std::env::var_os("THEGN_PLUGIN_TEST_HOLD").is_some() {
            loop {
                std::thread::park_timeout(std::time::Duration::from_secs(60));
            }
        }
    }

    #[test]
    fn dispatcher_thread_has_a_bounded_attribution_name() {
        let (name_tx, name_rx) = std::sync::mpsc::channel();
        let dispatcher = Dispatcher::spawn_with(std::time::Duration::from_secs(2), move |_, _| {
            let name = std::thread::current()
                .name()
                .map(str::to_owned)
                .expect("dispatcher thread has a name");
            name_tx.send(name).expect("test is listening");
            Box::pin(async { Ok(serde_json::Value::Null) })
        });

        let response = dispatch_request(&dispatcher, "plugin-a", 1)
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("dispatch response");
        assert!(response.error.is_none());
        assert_eq!(
            name_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("thread name"),
            "thegn-plugdisp"
        );
    }

    #[test]
    fn production_feed_bridge_thread_has_a_bounded_attribution_name() {
        let (name_tx, name_rx) = std::sync::mpsc::channel();
        let bridge = spawn_feed_bridge_worker(async move {
            let name = std::thread::current()
                .name()
                .map(str::to_owned)
                .expect("feed bridge thread has a name");
            name_tx.send(name).expect("test is listening");
        })
        .expect("feed bridge thread starts");
        let name = name_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("feed bridge thread name");
        assert_eq!(name, FEED_BRIDGE_THREAD);
        assert!(name.len() <= 15);
        drop(bridge);
    }

    fn dispatch_request(
        dispatcher: &Dispatcher,
        plugin: &str,
        id: u64,
    ) -> std::sync::mpsc::Receiver<RpcResponse> {
        let (reply, response) = test_reply();
        dispatcher.dispatch_with_reply(
            plugin,
            reply,
            id,
            "test.cap".into(),
            serde_json::Value::Null,
        );
        response
    }

    #[test]
    fn dispatcher_admission_is_bounded_and_plugin_fair() {
        let load = Arc::new(std::sync::Mutex::new(DispatchLoad::default()));
        let mut plugin_a = (0..MAX_DISPATCH_IN_FLIGHT_PER_PLUGIN)
            .map(|_| acquire_dispatch_permit(&load, "plugin-a").expect("within per-plugin cap"))
            .collect::<Vec<_>>();

        assert!(acquire_dispatch_permit(&load, "plugin-a").is_none());
        let plugin_b = acquire_dispatch_permit(&load, "plugin-b");
        assert!(
            plugin_b.is_some(),
            "plugin A cannot consume plugin B's limit"
        );

        drop(plugin_a.pop());
        assert!(acquire_dispatch_permit(&load, "plugin-a").is_some());
        drop(plugin_a);
        drop(plugin_b);
        let load = load.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(load.total, 0);
        assert!(load.per_plugin.is_empty());
    }

    #[test]
    fn dispatcher_global_admission_is_bounded() {
        let load = Arc::new(std::sync::Mutex::new(DispatchLoad::default()));
        let permits = (0..DISPATCH_QUEUE_CAPACITY)
            .map(|index| acquire_dispatch_permit(&load, &format!("plugin-{index}")))
            .collect::<Option<Vec<_>>>()
            .expect("global capacity is available");

        assert!(acquire_dispatch_permit(&load, "another-plugin").is_none());
        drop(permits);
        assert!(acquire_dispatch_permit(&load, "another-plugin").is_some());
    }

    #[test]
    fn dispatcher_diagnostics_are_taken_once_per_drain() {
        let mut diagnostics = DispatchDiagnosticsByPlugin::default();
        diagnostics.record("plugin-a", |counts| counts.rejected += 2);
        diagnostics.record("plugin-a", |counts| counts.timed_out += 1);

        let drained = diagnostics.take();
        let counts = drained.get("plugin-a").expect("recorded plugin");
        assert_eq!(counts.rejected, 2);
        assert_eq!(counts.timed_out, 1);
        assert!(diagnostics.take().is_empty());
    }

    #[test]
    fn dispatcher_replies_rate_limited_and_admits_other_plugins() {
        let (release, waiting) = tokio::sync::watch::channel(false);
        let dispatcher = Dispatcher::spawn_with(std::time::Duration::from_secs(10), move |_, _| {
            let mut waiting = waiting.clone();
            Box::pin(async move {
                waiting
                    .changed()
                    .await
                    .map_err(|error| RpcError::new(RpcErrorCode::Other, error.to_string()))?;
                Ok(serde_json::Value::Null)
            })
        });
        let in_flight = (0..MAX_DISPATCH_IN_FLIGHT_PER_PLUGIN)
            .map(|id| dispatch_request(&dispatcher, "plugin-a", id as u64))
            .collect::<Vec<_>>();

        let rejected = dispatch_request(&dispatcher, "plugin-a", 99);
        let response = rejected
            .recv_timeout(std::time::Duration::from_millis(500))
            .expect("rate limit response is immediate");
        assert_eq!(
            response.error.expect("error response").code,
            RpcErrorCode::RateLimited
        );
        let admitted_b = dispatch_request(&dispatcher, "plugin-b", 100);
        assert!(
            dispatcher
                .load
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .per_plugin
                .contains_key("plugin-b")
        );

        // Exercise the actual dispatch rejection path twice, then prove diagnostics flush once.
        let rejected_again = dispatch_request(&dispatcher, "plugin-a", 101);
        assert_eq!(
            rejected_again
                .recv_timeout(std::time::Duration::from_millis(500))
                .expect("second rejection")
                .error
                .expect("error response")
                .code,
            RpcErrorCode::RateLimited
        );
        let counts = dispatcher.take_diagnostics();
        assert_eq!(
            counts.get("plugin-a").expect("plugin diagnostics").rejected,
            2
        );
        assert!(dispatcher.take_diagnostics().is_empty());

        release.send(true).expect("workers are waiting");
        for response in in_flight.iter().chain(std::iter::once(&admitted_b)) {
            response
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("released dispatch completes");
        }
        assert_eq!(
            dispatcher
                .load
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .total,
            0
        );
    }

    #[test]
    fn dispatcher_timeout_replies_and_releases_permit() {
        let dispatcher =
            Dispatcher::spawn_with(std::time::Duration::from_millis(50), never_dispatch);
        let response = dispatch_request(&dispatcher, "plugin-a", 1)
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("timeout response");
        assert_eq!(
            response.error.expect("error response").code,
            RpcErrorCode::Timeout
        );
        assert_eq!(
            dispatcher
                .load
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .total,
            0
        );
    }

    #[test]
    fn disconnected_dispatcher_answers_other() {
        let dispatcher = Dispatcher::disconnected();
        let response = dispatch_request(&dispatcher, "plugin-a", 1)
            .recv_timeout(std::time::Duration::from_millis(500))
            .expect("disconnected response");
        assert_eq!(
            response.error.expect("error response").code,
            RpcErrorCode::Other
        );
    }

    #[test]
    fn dispatcher_skips_closed_sessions_and_counts_undelivered_replies() {
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_called = Arc::clone(&called);
        let (completed, completion) = std::sync::mpsc::channel();
        let dispatcher = Dispatcher::spawn_with(std::time::Duration::from_secs(2), move |_, _| {
            dispatch_called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let completed = completed.clone();
            Box::pin(async move {
                completed.send(()).expect("test is listening");
                Ok(serde_json::Value::Null)
            })
        });
        let closed = DispatchReply {
            respond: Box::new(|_| Err(SessionFailure::Closed)),
            is_closed: Box::new(|| true),
        };
        dispatcher.dispatch_with_reply(
            "plugin-a",
            closed,
            1,
            "test.cap".into(),
            serde_json::Value::Null,
        );
        let (replied, response) = std::sync::mpsc::channel();
        let reply = DispatchReply {
            respond: Box::new(move |_| {
                replied.send(()).expect("test is listening");
                Err(SessionFailure::Full)
            }),
            is_closed: Box::new(|| false),
        };
        dispatcher.dispatch_with_reply(
            "plugin-b",
            reply,
            2,
            "test.cap".into(),
            serde_json::Value::Null,
        );
        completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("later request passes the abandoned one");
        response
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("later dispatch reaches reply sink");
        dispatch_request(&dispatcher, "plugin-c", 3)
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("FIFO barrier completes after diagnostics update");
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 2);
        let diagnostics = dispatcher.take_diagnostics();
        assert_eq!(
            diagnostics
                .get("plugin-a")
                .expect("abandoned count")
                .abandoned,
            1
        );
        assert_eq!(
            diagnostics
                .get("plugin-b")
                .expect("undelivered count")
                .undelivered,
            1
        );
        assert_eq!(
            dispatcher
                .load
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .total,
            0
        );
    }

    fn contribution(id: &str, ep: ExtensionPoint, surface: &str, label: &str) -> Contribution {
        Contribution {
            id: ContributionId::new(id),
            extension_point: ep,
            label: label.into(),
            surface: Some(SurfaceId::new(surface)),
            cadence: CadenceHint::OnDemand,
            metadata: Default::default(),
            caps: serde_json::Value::Null,
            chord: None,
        }
    }

    fn spec(id: &str, contributions: Vec<Contribution>, scopes: Vec<Scope>) -> PluginSpec {
        let mut capabilities = contributions
            .iter()
            .filter_map(|contribution| {
                thegn_svc::plugin::loader::host_contract()
                    .support_for(&contribution.extension_point)
                    .and_then(|row| row.required_capability)
                    .and_then(Capability::parse)
            })
            .collect::<Vec<_>>();
        capabilities.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        capabilities.dedup();
        PluginSpec {
            manifest: PluginManifest {
                id: PluginId::new(id),
                name: id.into(),
                version: "1".into(),
                api: ApiVersion::new(0, 2, 0),
                capabilities,
                contributions,
            },
            command: vec!["true".into()],
            cwd: String::new(),
            env: Default::default(),
            timeout_secs: 5,
            scopes,
            mode: PluginMode::Resident,
            enabled: true,
            trusted_native: false,
        }
    }

    fn loaded_state(spec: PluginSpec) -> LoadedState {
        let negotiated = thegn_svc::plugin::loader::negotiate(&spec).expect("negotiates");
        LoadedState {
            plugin: LoadedPlugin {
                spec,
                dir: None,
                trusted_native_ignored: false,
            },
            negotiated,
            writer: None,
        }
    }

    fn state_with(specs: Vec<PluginSpec>) -> PluginsState {
        let mut state = PluginsState::new(thegn_core::config::Config::default());
        for s in specs {
            let id = s.manifest.id.as_str().to_string();
            state.plugins.insert(id, build_entry(loaded_state(s)));
        }
        state
    }

    fn seg_spec(id: &str) -> PluginSpec {
        spec(
            id,
            vec![contribution(
                "seg",
                ExtensionPoint::StatusBarSegment,
                &format!("{id}/seg"),
                &format!("{id} label"),
            )],
            Vec::new(),
        )
    }

    #[test]
    fn feed_bridge_claim_is_limited_to_one_per_entry() {
        let mut entry = build_entry(loaded_state(seg_spec("p")));
        entry.feed_bridge = spawn_cancellable_bridge("thegn-feedclaim", async {
            std::future::pending::<()>().await;
        });
        assert!(!claim_feed_bridge(&mut entry));
        let handle = Arc::clone(&entry.feed_bridge.as_ref().unwrap().handle);
        drop(entry.feed_bridge.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            handle.is_finished(),
            "dropping entry bridge must cancel its worker"
        );
        let (cancel, _receiver) = tokio::sync::oneshot::channel();
        entry.feed_bridge = Some(FeedBridge {
            handle,
            _cancel: cancel,
        });
        assert!(claim_feed_bridge(&mut entry));
    }

    #[test]
    fn dropping_feed_bridge_cancels_idle_worker() {
        let bridge = spawn_cancellable_bridge("thegn-feedcancl", async {
            std::future::pending::<()>().await;
        })
        .expect("worker thread starts");
        let handle = Arc::clone(&bridge.handle);
        drop(bridge);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            handle.is_finished(),
            "dropping bridge must cancel its idle worker"
        );
    }

    #[test]
    fn feed_bridge_admission_enforces_global_cap_and_same_plugin_idempotence() {
        let mut state = state_with(
            (0..=MAX_FEED_BRIDGES)
                .map(|i| seg_spec(&format!("p{i}")))
                .collect(),
        );
        for i in 0..MAX_FEED_BRIDGES {
            state.plugins.get_mut(&format!("p{i}")).unwrap().feed_bridge =
                spawn_cancellable_bridge("thegn-feedcap", async {
                    std::future::pending::<()>().await;
                });
        }
        assert!(!feed_bridge_admission(&state, "p0").unwrap());
        assert_eq!(
            feed_bridge_admission(&state, &format!("p{MAX_FEED_BRIDGES}"))
                .unwrap_err()
                .code,
            RpcErrorCode::RateLimited
        );
    }

    #[test]
    fn subscribe_verb_caps_custom_kinds_per_plugin() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        for start in [0, 40] {
            let events = (start..start + 40)
                .map(|i| serde_json::to_value(EventKind::Custom(format!("custom-{i}"))).unwrap())
                .collect::<Vec<_>>();
            drain_one(
                &mut state,
                &mut model,
                PluginMsg::Event {
                    plugin: "p".into(),
                    event: SessionEvent::Message(msg(
                        "subscribe",
                        serde_json::json!({ "events": events }),
                    )),
                },
            );
        }
        assert_eq!(
            state.plugins["p"].runtime.subscriptions().len(),
            MAX_SUBSCRIPTIONS_PER_PLUGIN
        );

        subscribe_kinds(
            &mut state.plugins.get_mut("p").unwrap().runtime,
            PluginId::new("p"),
            vec![EventKind::Custom("custom-0".into())],
        )
        .expect("an existing subscription is accepted at the cap");
    }

    fn msg(method: &str, params: serde_json::Value) -> RpcMessage {
        RpcMessage {
            id: None,
            method: method.into(),
            params,
        }
    }

    fn drain_one(state: &mut PluginsState, model: &mut FrameModel, m: PluginMsg) -> PluginDamage {
        drain_messages(state, model, [m])
    }

    fn drain_messages(
        state: &mut PluginsState,
        model: &mut FrameModel,
        messages: impl IntoIterator<Item = PluginMsg>,
    ) -> PluginDamage {
        let (tx, mut rx) = crate::plugins::plugin_channel();
        for message in messages {
            tx.send(message).unwrap();
        }
        drop(tx);
        drain(&mut rx, state, model, None)
    }

    fn update_message(text: String) -> PluginMsg {
        PluginMsg::Event {
            plugin: "p".into(),
            event: SessionEvent::Message(msg(
                "update",
                serde_json::json!({
                    "surface": "p/seg",
                    "view": {"spans": [{"text": text, "role": "Default"}]}
                }),
            )),
        }
    }

    #[test]
    fn plugin_drain_budget_preserves_fifo_backlog_for_the_next_call() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        let (tx, mut rx) = crate::plugins::plugin_channel();
        for index in 0..MAX_PLUGIN_MESSAGES_PER_DRAIN + 4 {
            tx.send(PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Message(msg(
                    "emit",
                    serde_json::json!({"topic":"test","data":{"index":index}}),
                )),
            })
            .unwrap();
        }
        tx.send(update_message(format!(
            "update-{}",
            MAX_PLUGIN_MESSAGES_PER_DRAIN + 4
        )))
        .unwrap();

        drain(&mut rx, &mut state, &mut model, None);
        assert_eq!(rx.len(), 5, "the first drain stops at its message budget");

        drain(&mut rx, &mut state, &mut model, None);
        assert!(rx.is_empty());
        assert_eq!(
            statusbar_views(&state)[0].1.text_content(),
            format!("update-{}", MAX_PLUGIN_MESSAGES_PER_DRAIN + 4)
        );
    }

    #[test]
    fn flooded_request_gets_rate_limited_response() {
        let mut state = state_with(vec![seg_spec("p")]);
        let original = statusbar_views(&state);
        let mut model = FrameModel::default();
        let (tx, mut rx) = crate::plugins::plugin_channel();
        for index in 0..crate::plugins::inbox::MAX_QUEUED_EVENTS_PER_PLUGIN {
            tx.send(PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Message(msg(
                    "emit",
                    serde_json::json!({"topic":"flood","data":index}),
                )),
            })
            .unwrap();
        }
        tx.send(PluginMsg::Event {
            plugin: "p".into(),
            event: SessionEvent::Message(RpcMessage {
                id: Some(77),
                method: "io".into(),
                params: serde_json::json!({}),
            }),
        })
        .unwrap();

        let mut responses = Vec::new();
        while !rx.is_empty() {
            drain_with_responder(&mut rx, &mut state, &mut model, None, &mut |_, response| {
                responses.push(response)
            });
        }

        let diagnostics = state.inbox.as_ref().unwrap().take_diagnostics();
        assert_eq!(diagnostics["p"].rejected, 1);
        assert!(
            rx.is_empty(),
            "the rejected request is consumed by the drain"
        );
        assert_eq!(
            responses,
            [RpcResponse::err(
                77,
                RpcError::new(RpcErrorCode::RateLimited, "host plugin queue full")
            )]
        );
        assert_eq!(statusbar_views(&state), original);
        assert!(state.plugins.contains_key("p"));
    }

    #[test]
    fn respawn_lifecycle_is_processed_before_rejected_request() {
        let (tx, mut rx) = crate::plugins::plugin_channel();
        for _ in 0..crate::plugins::inbox::MAX_QUEUED_EVENTS_PER_PLUGIN {
            rx.inbox().push(
                "p".into(),
                SessionEvent::Message(msg("emit", serde_json::json!({}))),
            );
        }
        assert_eq!(
            rx.inbox().push(
                "p".into(),
                SessionEvent::Message(RpcMessage {
                    id: Some(77),
                    method: "io".into(),
                    params: serde_json::json!({}),
                }),
            ),
            crate::plugins::inbox::PushOutcome::Rejected
        );
        tx.send(PluginMsg::Event {
            plugin: "p".into(),
            event: SessionEvent::Exit { code: None },
        })
        .unwrap();
        tx.send(PluginMsg::Respawned {
            plugin: "p".into(),
            writer: None,
        })
        .unwrap();

        assert!(matches!(
            rx.try_recv(),
            Ok(PluginMsg::Event {
                event: SessionEvent::Exit { .. },
                ..
            })
        ));
        assert!(matches!(rx.try_recv(), Ok(PluginMsg::Respawned { .. })));
        assert_eq!(rx.pop_rejected_if_lifecycle_empty().unwrap().id, 77);
    }

    #[test]
    fn update_flood_from_session_keeps_memory_bounded() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        let (tx, mut rx) = crate::plugins::plugin_channel();
        for index in 0..100_000 {
            tx.send(update_message(format!("flood-{index}"))).unwrap();
        }
        assert_eq!(rx.len(), 1);

        drain(&mut rx, &mut state, &mut model, None);

        assert_eq!(statusbar_views(&state)[0].1.text_content(), "flood-99999");
    }

    #[test]
    fn one_shot_larger_than_budget_is_applied_whole() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        let run = thegn_svc::plugin::PluginRun {
            messages: (0..MAX_PLUGIN_MESSAGES_PER_DRAIN + 5)
                .map(|index| {
                    msg(
                        "update",
                        serde_json::json!({
                            "surface": "p/seg",
                            "view": {"spans": [{"text": format!("shot-{index}"), "role": "Default"}]}
                        }),
                    )
                })
                .collect(),
            ..Default::default()
        };
        let (tx, mut rx) = crate::plugins::plugin_channel();
        tx.send(PluginMsg::OneShot {
            plugin: "p".into(),
            run: Ok(run),
        })
        .unwrap();
        tx.send(update_message("after-shot".into())).unwrap();
        drop(tx);

        drain(&mut rx, &mut state, &mut model, None);

        assert_eq!(rx.len(), 1, "the oversized one-shot is one atomic unit");
        assert_eq!(
            statusbar_views(&state)[0].1.text_content(),
            format!("shot-{}", MAX_PLUGIN_MESSAGES_PER_DRAIN + 4)
        );

        drain(&mut rx, &mut state, &mut model, None);
        assert!(rx.is_empty());
        assert_eq!(statusbar_views(&state)[0].1.text_content(), "after-shot");
    }

    #[test]
    fn update_verb_caches_a_view_and_requests_repaint() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        let repaint = drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Message(msg(
                    "update",
                    serde_json::json!({
                        "surface": "p/seg",
                        "view": {"spans": [{"text": "3 mails", "role": "Default"}]}
                    }),
                )),
            },
        );
        assert_ne!(repaint, PluginDamage::None, "changed view repaints");
        let views = statusbar_views(&state);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].0, "p label");
        assert_eq!(views[0].1.text_content(), "3 mails");
        // Equal-width content update stays on the bounded statusbar path.
        let repaint = drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Message(msg(
                    "update",
                    serde_json::json!({
                        "surface": "p/seg",
                        "view": {"spans": [{"text": "4 mails", "role": "Default"}]}
                    }),
                )),
            },
        );
        assert_eq!(repaint, PluginDamage::StatusbarContent);

        // Identical update → no repaint (SurfaceCache reports unchanged).
        let repaint = drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Message(msg(
                    "update",
                    serde_json::json!({
                        "surface": "p/seg",
                        "view": {"spans": [{"text": "4 mails", "role": "Default"}]}
                    }),
                )),
            },
        );
        assert_eq!(
            repaint,
            PluginDamage::None,
            "unchanged view is not a repaint"
        );

        // Coalesced updates that return to the visible view before the frame
        // boundary also emit no frame.
        let repaint = drain_messages(
            &mut state,
            &mut model,
            [
                PluginMsg::Event {
                    plugin: "p".into(),
                    event: SessionEvent::Message(msg(
                        "update",
                        serde_json::json!({
                            "surface": "p/seg",
                            "view": {"spans": [{"text": "3 mails", "role": "Default"}]}
                        }),
                    )),
                },
                PluginMsg::Event {
                    plugin: "p".into(),
                    event: SessionEvent::Message(msg(
                        "update",
                        serde_json::json!({
                            "surface": "p/seg",
                            "view": {"spans": [{"text": "4 mails", "role": "Default"}]}
                        }),
                    )),
                },
            ],
        );
        assert_eq!(repaint, PluginDamage::None);
    }

    #[test]
    fn one_shot_messages_apply_like_session_messages() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        let run = thegn_svc::plugin::PluginRun {
            messages: vec![msg(
                "update",
                serde_json::json!({
                    "surface": "p/seg",
                    "view": {"spans": [{"text": "ok", "role": "Accent"}]}
                }),
            )],
            ..Default::default()
        };
        assert_ne!(
            drain_one(
                &mut state,
                &mut model,
                PluginMsg::OneShot {
                    plugin: "p".into(),
                    run: Ok(run)
                }
            ),
            PluginDamage::None
        );
        assert_eq!(statusbar_views(&state)[0].1.text_content(), "ok");
    }

    #[test]
    fn notify_from_a_notification_source_lands_in_the_alert_outbox() {
        let mut state = state_with(vec![spec(
            "n",
            vec![contribution(
                "src",
                ExtensionPoint::NotificationSource,
                "n/alerts",
                "N",
            )],
            Vec::new(),
        )]);
        let mut model = FrameModel::default();
        drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "n".into(),
                event: SessionEvent::Message(msg(
                    "notify",
                    serde_json::json!({"source": "n", "message": "build red"}),
                )),
            },
        );
        assert_eq!(state.pending_alerts.len(), 1);
        assert_eq!(state.pending_alerts[0].1.message, "build red");
        // Audit shows the grant.
        let entry = &state.plugins["n"];
        assert!(matches!(
            entry.runtime.audit_log().last().map(|e| e.decision),
            Some(thegn_core::plugin_api::AuditDecision::Granted)
        ));
    }

    #[test]
    fn alert_outbox_caps_each_plugin_without_starving_other_plugins() {
        let source = |id: &str| {
            spec(
                id,
                vec![contribution(
                    "src",
                    ExtensionPoint::NotificationSource,
                    &format!("{id}/alerts"),
                    id,
                )],
                Vec::new(),
            )
        };
        let mut state = state_with(vec![source("n"), source("m")]);
        let mut model = FrameModel::default();
        let notify = |plugin: &str| PluginMsg::Event {
            plugin: plugin.into(),
            event: SessionEvent::Message(msg(
                "notify",
                serde_json::json!({"source": plugin, "message": "界".repeat(500)}),
            )),
        };

        drain_messages(
            &mut state,
            &mut model,
            (0..100).map(|_| notify("n")).chain([notify("m")]),
        );

        let n_alerts = state
            .pending_alerts
            .iter()
            .filter(|(plugin, _)| plugin == "n")
            .count();
        assert_eq!(n_alerts, MAX_PENDING_ALERTS_PER_PLUGIN);
        assert!(state.pending_alerts.iter().any(|(plugin, alert)| {
            plugin == "m" && alert.message.len() == MAX_ALERT_MESSAGE_BYTES - 1
        }));
        assert!(
            state
                .pending_alerts
                .iter()
                .all(|(_, alert)| alert.message.len() <= MAX_ALERT_MESSAGE_BYTES)
        );
    }

    #[test]
    fn initial_resident_spawn_failure_enters_restart_backoff() {
        let mut state = PluginsState::new(thegn_core::config::Config::default());
        let mut model = FrameModel::default();

        drain_one(
            &mut state,
            &mut model,
            PluginMsg::Loaded(vec![loaded_state(seg_spec("p"))]),
        );

        assert_eq!(state.plugins["p"].restarts, 1);
        assert!(state.plugins["p"].writer.is_none());
    }

    #[test]
    fn notify_without_a_notification_source_is_denied_and_audited() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Message(msg(
                    "notify",
                    serde_json::json!({"source": "p", "message": "sneaky"}),
                )),
            },
        );
        assert!(
            state.pending_alerts.is_empty(),
            "denied alert is not queued"
        );
        let entry = &state.plugins["p"];
        assert!(matches!(
            entry.runtime.audit_log().last().map(|e| e.decision),
            Some(thegn_core::plugin_api::AuditDecision::Denied)
        ));
    }

    #[test]
    fn state_set_then_get_round_trips_through_the_runtime() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Message(msg(
                    "state.set",
                    serde_json::json!({"key": "cursor", "value": 7}),
                )),
            },
        );
        let entry = state.plugins.get_mut("p").unwrap();
        let got = entry
            .runtime
            .state_get(PluginId::new("p"), "cursor")
            .unwrap();
        assert_eq!(got, Some(serde_json::json!(7)));
    }

    #[test]
    fn register_accepts_the_negotiated_contribution_idempotently() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        let c = state.plugins["p"].contributions[0].clone();
        for _ in 0..2 {
            drain_one(
                &mut state,
                &mut model,
                PluginMsg::Event {
                    plugin: "p".into(),
                    event: SessionEvent::Message(msg(
                        "register",
                        serde_json::to_value(&c).unwrap(),
                    )),
                },
            );
        }
        assert_eq!(state.plugins["p"].contributions.len(), 1);
        let audit = state.plugins["p"].runtime.audit_log();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].decision, AuditDecision::Granted);
    }

    #[test]
    fn unsupported_io_uses_the_wire_unsupported_error_code() {
        let error = rpc_error_of(PluginApiError::UnsupportedIo("network".into()));
        assert_eq!(error.code, RpcErrorCode::Unsupported);
        assert!(error.message.contains("network"));
    }

    #[test]
    fn quota_exceeded_uses_the_wire_invalid_error_code() {
        let error = rpc_error_of(PluginApiError::QuotaExceeded {
            what: "state value",
            size: 70_000,
            limit: 65_536,
        });
        assert_eq!(error.code, RpcErrorCode::Invalid);
        assert!(error.message.contains("state value"));
    }

    #[test]
    fn register_rejects_nested_params_with_typed_invalid_diagnostic() {
        let err = parse_register_contribution(serde_json::json!({
            "plugin": "hello",
            "contribution": {
                "id": "hello.seg",
                "extension_point": "StatusBarSegment",
                "label": "Hello",
                "surface": "hello.segment"
            }
        }))
        .expect_err("register params must be the flat Contribution object");

        assert_eq!(err.code, RpcErrorCode::Invalid);
        assert!(err.message.starts_with("bad contribution:"), "{err:?}");
    }

    #[test]
    fn folder_assignment_needs_the_git_scope_for_plugins() {
        let bare = seg_spec("p");
        let err = host_call_check(&bare, "folders.assign").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Denied);
        assert!(err.message.contains("Git"), "{err:?}");
        let git = spec("p", Vec::new(), vec![Scope::Git]);
        let read = spec("p", Vec::new(), vec![Scope::Read, Scope::Write]);
        assert_eq!(
            host_call_check(&read, "folders.assign").unwrap_err().code,
            RpcErrorCode::Denied
        );
        assert!(host_call_check(&git, "folders.assign").is_ok());
    }

    #[test]
    fn scope_check_denies_dispatches_and_flags_unknown_caps() {
        // No scopes → Denied.
        let bare = seg_spec("p");
        let err = host_call_check(&bare, "sessions.list").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Denied);
        let mut entry = build_entry(loaded_state(bare.clone()));
        let err =
            authorize_host_call(&mut entry, "sessions.list", &serde_json::Value::Null).unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Denied);
        let audit = entry.runtime.audit_log().last().unwrap();
        assert_eq!(audit.decision, AuditDecision::Denied);
        assert_eq!(audit.operation, "host.call");
        assert_eq!(audit.capability.as_str(), "host:sessions.list");
        assert_eq!(audit.target, None);
        // Read scope → every read-verb plugin cap passes the scope check.
        let read = spec("p", Vec::new(), vec![Scope::Read]);
        for cap in thegn_core::plugin_api::plugin_host_call_caps() {
            let need = thegn_core::control::required_scope(
                thegn_core::capability::lookup(cap).unwrap().verb,
            );
            if need == Scope::Read {
                assert!(host_call_check(&read, cap).is_ok(), "{cap}");
            }
        }
        // A write cap needs the write scope: denied for a read-only plugin.
        let err = host_call_check(&read, "sessions.kill").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Denied);
        // Exec is independent from both write and surface capabilities.
        for scopes in [vec![Scope::Read], vec![Scope::Write], vec![Scope::Git]] {
            let without_exec = spec("p", Vec::new(), scopes);
            let err = host_call_check(&without_exec, "tools.run").unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Denied);
        }
        let exec = spec("p", Vec::new(), vec![Scope::Exec]);
        assert!(host_call_check(&exec, "tools.run").is_ok());
        // launch.preset has no generic control route yet and is deliberately
        // absent from the plugin surface even for an exec-scoped plugin.
        let err = host_call_check(&exec, "launch.preset").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Unsupported);
        // A read cap the generic dispatcher now serves passes.
        assert!(host_call_check(&read, "sessions.snapshot").is_ok());
        // Unknown cap → Invalid.
        let err = host_call_check(&read, "wibble.frobnicate").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Invalid);
        // Admin rows are unreachable by construction: they never list the
        // plugin surface, so even an admin-scoped plugin gets Unsupported.
        let admin = spec("p", Vec::new(), vec![Scope::Admin]);
        for cap in ["pairings.issue", "daemon.shutdown"] {
            let err = host_call_check(&admin, cap).unwrap_err();
            assert_eq!(err.code, RpcErrorCode::Unsupported, "{cap}");
        }
    }

    #[test]
    fn merge_add_audit_records_only_its_worktree_target_for_allowed_and_denied_calls() {
        let target = serde_json::json!({ "worktree": "wt-a" });
        let git = spec("p", Vec::new(), vec![Scope::Git]);
        let mut allowed = build_entry(loaded_state(git));
        authorize_host_call(&mut allowed, "merge.add", &target).unwrap();
        let audit = allowed.runtime.audit_log().last().unwrap();
        assert_eq!(audit.decision, AuditDecision::Granted);
        assert_eq!(audit.target.as_deref(), Some("wt-a"));

        let bare = seg_spec("p");
        let mut denied = build_entry(loaded_state(bare));
        assert!(authorize_host_call(&mut denied, "merge.add", &target).is_err());
        let audit = denied.runtime.audit_log().last().unwrap();
        assert_eq!(audit.decision, AuditDecision::Denied);
        assert_eq!(audit.target.as_deref(), Some("wt-a"));

        let read = spec("p", Vec::new(), vec![Scope::Read]);
        let mut unrelated = build_entry(loaded_state(read));
        authorize_host_call(
            &mut unrelated,
            "sessions.list",
            &serde_json::json!({ "secret": "must-not-be-copied" }),
        )
        .unwrap();
        assert_eq!(unrelated.runtime.audit_log().last().unwrap().target, None);
    }

    #[test]
    fn merge_add_requires_git_scope_not_naked_merge_scope() {
        let merge = spec("p", Vec::new(), vec![Scope::MergeAdd]);
        assert_eq!(
            host_call_check(&merge, "merge.add").unwrap_err().code,
            RpcErrorCode::Denied
        );
        let git = spec("p", Vec::new(), vec![Scope::Git]);
        assert!(host_call_check(&git, "merge.add").is_ok());
    }

    #[test]
    fn host_verb_modes_follow_the_canonical_support_matrix() {
        let resident = seg_spec("resident");
        let mut one_shot = seg_spec("one-shot");
        one_shot.mode = PluginMode::OneShot;
        for row in HOST_VERB_SUPPORT {
            assert_eq!(
                host_verb_check(&resident, row.verb).is_ok(),
                row.state == SupportState::Wired && row.modes.contains(&PluginMode::Resident),
                "resident mode drift for {}",
                row.verb.method_name()
            );
            assert_eq!(
                host_verb_check(&one_shot, row.verb).is_ok(),
                row.state == SupportState::Wired && row.modes.contains(&PluginMode::OneShot),
                "one-shot mode drift for {}",
                row.verb.method_name()
            );
        }
    }

    #[test]
    fn palette_actions_list_and_route_to_their_owner() {
        assert_eq!(
            one_shot_status("one-shot", "one-shot.hello", OneShotStart::Busy),
            "Plugin one-shot: one-shot.hello is already running"
        );
        assert_eq!(
            one_shot_status("one-shot", "one-shot.hello", OneShotStart::Failed),
            "Plugin one-shot: could not start one-shot.hello"
        );
        assert_eq!(
            one_shot_status("one-shot", "one-shot.hello", OneShotStart::Overloaded),
            format!(
                "Plugin one-shot: one-shot.hello not started: too many plugin runs in flight ({} max)",
                crate::plugins::MAX_INFLIGHT_ONE_SHOTS
            )
        );
        let mut state = state_with(vec![spec(
            "acts",
            vec![contribution(
                "acts.hello",
                ExtensionPoint::PaletteAction,
                "acts/palette",
                "Say hello",
            )],
            Vec::new(),
        )]);
        let items = palette_items(&state);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].key, "plugin:acts:acts.hello");
        assert!(items[0].label.contains("Say hello"), "{items:?}");

        // A resident with no session reports health; writer absence must not
        // route it through one-shot execution.
        assert!(
            invoke_palette_action(&state, None, "plugin:acts:acts.hello")
                .is_some_and(|status| status.contains("not running"))
        );

        // The same missing-writer state for a configured one-shot keeps the
        // prior behavior when no host handle is available.
        let mut one_shot = spec(
            "one-shot",
            vec![contribution(
                "one-shot.hello",
                ExtensionPoint::PaletteAction,
                "one-shot/palette",
                "Say hello",
            )],
            Vec::new(),
        );
        one_shot.mode = PluginMode::OneShot;
        let one_shot_state = state_with(vec![one_shot]);
        assert_eq!(
            invoke_palette_action(&one_shot_state, None, "plugin:one-shot:one-shot.hello"),
            None
        );
        // Unknown contribution / unknown plugin answer with a status.
        assert!(
            invoke_palette_action(&state, None, "plugin:acts:nope")
                .is_some_and(|s| s.contains("no action")),
        );
        assert_eq!(invoke_palette_action(&state, None, "plugin:ghost:x"), None);
        // Disabled plugins vanish from the palette and refuse invocation.
        state.plugins.get_mut("acts").unwrap().disabled = true;
        assert!(palette_items(&state).is_empty());
        assert!(
            invoke_palette_action(&state, None, "plugin:acts:acts.hello")
                .is_some_and(|s| s.contains("disabled")),
        );
    }

    #[test]
    fn statusbar_views_are_stable_by_plugin_id_then_contribution_order() {
        let mut b = seg_spec("b-plug");
        b.manifest.contributions.push(contribution(
            "second",
            ExtensionPoint::StatusBarSegment,
            "b-plug/second",
            "b second",
        ));
        let mut state = state_with(vec![b, seg_spec("a-plug")]);
        let mut model = FrameModel::default();
        for (plugin, surface) in [
            ("b-plug", "b-plug/second"),
            ("b-plug", "b-plug/seg"),
            ("a-plug", "a-plug/seg"),
        ] {
            drain_one(
                &mut state,
                &mut model,
                PluginMsg::Event {
                    plugin: plugin.into(),
                    event: SessionEvent::Message(msg(
                        "update",
                        serde_json::json!({
                            "surface": surface,
                            "view": {"spans": [{"text": surface, "role": "Default"}]}
                        }),
                    )),
                },
            );
        }
        let views = statusbar_views(&state);
        let labels: Vec<String> = views.iter().map(|(l, _)| l.clone()).collect();
        assert_eq!(labels, ["a-plug label", "b-plug label", "b second"]);
    }

    #[test]
    fn exit_backs_off_then_disables_with_a_status_line() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        for i in 1..=3u32 {
            drain_one(
                &mut state,
                &mut model,
                PluginMsg::Event {
                    plugin: "p".into(),
                    event: SessionEvent::Exit { code: Some(1) },
                },
            );
            assert_eq!(state.plugins["p"].restarts, i);
            assert!(!state.plugins["p"].disabled);
        }
        // Fourth crash exceeds the cap: disabled + user-visible status.
        drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "p".into(),
                event: SessionEvent::Exit { code: Some(1) },
            },
        );
        assert!(state.plugins["p"].disabled);
        assert!(model.status.contains("plugin p"), "{}", model.status);
        assert!(
            statusbar_views(&state).is_empty(),
            "disabled plugins render nothing"
        );
    }

    #[test]
    fn effective_grants_cover_surfaces_state_and_notify() {
        let s = spec(
            "p",
            vec![
                contribution("a", ExtensionPoint::StatusBarSegment, "p/seg", "A"),
                contribution("b", ExtensionPoint::NotificationSource, "p/alerts", "B"),
            ],
            Vec::new(),
        );
        let neg = thegn_svc::plugin::loader::negotiate(&s).unwrap();
        let grants = effective_grants(&neg, &s);
        for want in [
            Capability::new("surface", "statusbar"),
            Capability::new("surface", "notification"),
            Capability::new("state", "p"),
            Capability::new("notify", "p"),
        ] {
            assert!(grants.contains(&want), "{want}");
        }
    }

    #[test]
    fn unknown_plugin_and_junk_are_ignored() {
        let mut state = state_with(vec![]);
        let mut model = FrameModel::default();
        // drain_one now returns PluginDamage, not bool: an unknown plugin must
        // produce NO damage at all, which is what keeps the loop idle.
        assert_eq!(
            drain_one(
                &mut state,
                &mut model,
                PluginMsg::Event {
                    plugin: "ghost".into(),
                    event: SessionEvent::Message(msg("update", serde_json::json!({}))),
                }
            ),
            PluginDamage::None
        );
        assert_eq!(
            drain_one(
                &mut state,
                &mut model,
                PluginMsg::Event {
                    plugin: "ghost".into(),
                    event: SessionEvent::Junk("println! debris".into()),
                }
            ),
            PluginDamage::None
        );
    }

    #[test]
    fn views_survive_and_render_spans() {
        // Sanity on the View plumbing the chrome consumes.
        let v = View::line([
            Span::styled("a", StyleRole::Accent),
            Span::styled("b", StyleRole::Default),
        ]);
        assert_eq!(v.text_content(), "ab");
    }

    #[test]
    fn refused_message_sets_status_naming_plugin_and_flag() {
        let mut state = PluginsState::new(thegn_core::config::Config::default());
        let mut model = FrameModel::default();
        let damage = drain_one(
            &mut state,
            &mut model,
            PluginMsg::Refused(vec![
                thegn_svc::plugin::loader::SpecProblem {
                    plugin: "untrusted-plugin".into(),
                    problem: "refused: declares network:example.com; add trusted_native = true"
                        .into(),
                },
                thegn_svc::plugin::loader::SpecProblem {
                    plugin: "second-plugin".into(),
                    problem: "refused: declares run:git; add trusted_native = true".into(),
                },
            ]),
        );
        assert!(model.status.contains("untrusted-plugin"));
        assert!(model.status.contains("trusted_native = true"));
        assert!(model.status.contains("(+1 more; see thegn plugin check)"));
        assert_ne!(damage, PluginDamage::None);
    }
}
