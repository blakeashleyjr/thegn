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

use thegn_core::plugin_api::{
    API_VERSION, Alert, Capability, Contribution, Event, EventKind, ExtensionPoint, HostVerb,
    IoRequest, NegotiatedManifest, PluginApiError, PluginId, PluginRuntime, PluginSpec, RpcError,
    RpcErrorCode, RpcMessage, RpcResponse, SurfaceId, View,
};
use thegn_svc::issue::IssueCaps;
use thegn_svc::plugin::{LoadedPlugin, SessionEvent, SessionWriter};
use tokio::sync::mpsc as tokio_mpsc;

use crate::chrome::FrameModel;
use crate::plugins::{LoadedState, PluginMsg, PluginsHost};

/// One live (or dead-but-remembered) plugin on the loop side.
pub(crate) struct PluginEntry {
    pub plugin: LoadedPlugin,
    pub runtime: PluginRuntime,
    /// `Some` while a resident session is alive; `None` for one-shot plugins
    /// and dead residents.
    pub writer: Option<SessionWriter>,
    /// Contributions currently in effect: the negotiated set plus any the
    /// plugin `register`ed at runtime.
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
    /// Lazily started `host.call` dispatcher (a thread is only spawned once a
    /// plugin actually calls).
    dispatcher: Option<Dispatcher>,
    /// Owned config for the dispatcher's daemon discovery.
    cfg: thegn_core::config::Config,
}

impl PluginsState {
    pub(crate) fn new(cfg: thegn_core::config::Config) -> Self {
        Self {
            plugins: BTreeMap::new(),
            pending_alerts: Vec::new(),
            dispatcher: None,
            cfg,
        }
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
        Ok(())
    } else {
        Err(RpcError::new(
            RpcErrorCode::Denied,
            format!("{cap} requires the {need:?} scope, which this plugin was not granted"),
        ))
    }
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
                thegn_svc::plugin::ProviderBridge::new(
                    w.clone(),
                    std::time::Duration::from_secs(st.plugin.spec.timeout_secs.max(1)),
                )
            })
    });
    PluginEntry {
        runtime: PluginRuntime::new(negotiated),
        plugin: st.plugin,
        writer: st.writer,
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
/// key. Residents get an `on_event` notification (`kind: Action`,
/// `payload.id` = the contribution); one-shot plugins run once, now, via
/// [`crate::plugins::PluginsHost::run_one_shot`]. Returns a status line.
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
    match &entry.writer {
        Some(writer) => {
            if let Err(e) = writer.notify(
                thegn_core::plugin_api::PluginCallback::OnEvent,
                serde_json::to_value(&event).unwrap_or_default(),
            ) {
                return Some(format!("Plugin {plugin}: {e}"));
            }
            Some(format!("Plugin {plugin}: {contribution}"))
        }
        None => {
            host?.run_one_shot(entry.plugin.clone());
            Some(format!("Plugin {plugin}: running {contribution}"))
        }
    }
}

/// Record this drain's raised alerts to the notification inbox, off-loop.
/// Call after [`drain`] from the event loop (requires a tokio runtime).
pub(crate) fn flush_alerts(state: &mut PluginsState) {
    for (plugin, alert) in state.pending_alerts.drain(..) {
        tokio::task::spawn_blocking(move || {
            use thegn_core::store::NotificationStore;
            let Ok(db) = thegn_core::db::Db::open() else {
                return;
            };
            // best-effort: the inbox is a cache; the audit log has the record.
            let _ = db.put_notification(
                "plugin",
                &plugin,
                &format!("{}: {}", alert.source, alert.message),
                "",
            );
        });
    }
}

/// Apply every queued [`PluginMsg`]. Returns whether chrome must repaint (a
/// statusbar view changed, a contribution set changed, or a plugin died).
/// `host` is the respawn/shutdown handle; `None` (tests) records restart
/// decisions without spawning anything.
pub(crate) fn drain(
    rx: &mut tokio_mpsc::UnboundedReceiver<PluginMsg>,
    state: &mut PluginsState,
    model: &mut FrameModel,
    host: Option<&PluginsHost>,
) -> bool {
    let mut repaint = false;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            PluginMsg::Loaded(list) => {
                for st in list {
                    let id = st.plugin.spec.manifest.id.as_str().to_string();
                    let entry = build_entry(st);
                    send_activate(&entry);
                    state.plugins.insert(id, entry);
                }
                sync_provider_registry(state);
                repaint = true;
            }
            PluginMsg::Event { plugin, event } => match event {
                SessionEvent::Message(m) => repaint |= apply_message(state, &plugin, m),
                SessionEvent::Response(r) => {
                    // Provider bridge replies resolve their waiting seam
                    // call; anything else is a stray.
                    let routed = state
                        .plugins
                        .get(&plugin)
                        .and_then(|e| e.bridge.as_ref())
                        .is_some_and(|b| b.resolve(r.clone()));
                    if !routed {
                        tracing::debug!(target: "thegn::plugin", plugin = %plugin, id = r.id, "response from plugin (no host request pending)");
                    }
                }
                SessionEvent::Junk(line) => {
                    tracing::debug!(target: "thegn::plugin", plugin = %plugin, "junk: {line}");
                }
                SessionEvent::Exit { code } => {
                    repaint |= handle_exit(state, model, host, &plugin, code);
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
                        repaint |= apply_message(state, &plugin, m);
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
                        if entry
                            .contributions
                            .iter()
                            .any(|c| c.extension_point == ExtensionPoint::IssueProvider)
                        {
                            entry.bridge = Some(thegn_svc::plugin::ProviderBridge::new(
                                w.clone(),
                                std::time::Duration::from_secs(
                                    entry.plugin.spec.timeout_secs.max(1),
                                ),
                            ));
                        }
                        entry.writer = Some(w);
                        send_activate(entry);
                        sync_provider_registry(state);
                        repaint = true;
                    }
                    // The respawn itself failed: treat like another crash so
                    // the backoff keeps climbing toward the cap.
                    None => repaint |= handle_exit(state, model, host, &plugin, None),
                }
            }
        }
    }
    repaint
}

/// A resident session died: decide restart (backoff, capped at 3) or mark the
/// plugin disabled-until-reload.
fn handle_exit(
    state: &mut PluginsState,
    model: &mut FrameModel,
    host: Option<&PluginsHost>,
    plugin: &str,
    code: Option<i32>,
) -> bool {
    let Some(entry) = state.plugins.get_mut(plugin) else {
        return false;
    };
    entry.writer = None;
    entry.bridge = None;
    sync_provider_registry(state);
    let entry = state.plugins.get_mut(plugin).expect("looked up above");
    if entry.disabled {
        return false;
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
                host.respawn(entry.plugin.clone(), delay);
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
    true
}

/// Map a runtime error onto the wire vocabulary.
fn rpc_error_of(e: PluginApiError) -> RpcError {
    let code = match &e {
        PluginApiError::CapabilityDenied { .. } => RpcErrorCode::Denied,
        PluginApiError::UnknownExtensionPoint(_) => RpcErrorCode::Invalid,
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

/// Apply one verb from a plugin. Returns whether chrome must repaint.
fn apply_message(state: &mut PluginsState, plugin: &str, msg: RpcMessage) -> bool {
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
        return false;
    };
    // host.call needs `&mut state` for the lazy dispatcher, so it is handled
    // before the per-entry borrow below.
    if verb == HostVerb::HostCall {
        return apply_host_call(state, plugin, msg);
    }
    let Some(entry) = state.plugins.get_mut(plugin) else {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, "message from unknown plugin");
        return false;
    };
    let pid = PluginId::new(plugin);
    let params = msg.params;
    // The verb's effect, plus the value (if any) an id-bearing request gets
    // back. Invalid params short-circuit to an Invalid error response.
    let mut repaint = false;
    let outcome: Result<serde_json::Value, RpcError> = match verb {
        HostVerb::Register => serde_json::from_value::<Contribution>(params)
            .map_err(|e| RpcError::new(RpcErrorCode::Invalid, format!("bad contribution: {e}")))
            .and_then(|c| {
                entry
                    .runtime
                    .register(pid.clone(), c.clone())
                    .map_err(rpc_error_of)?;
                if !entry.contributions.iter().any(|x| x.id == c.id) {
                    repaint |= c.extension_point == ExtensionPoint::StatusBarSegment;
                    entry.contributions.push(c);
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
                    .map(|res| {
                        repaint |= res.changed;
                        serde_json::Value::Null
                    }),
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
                Ok(kinds) => {
                    let mut r = Ok(serde_json::Value::Null);
                    for k in kinds {
                        if let Err(e) = entry.runtime.subscribe(pid.clone(), k) {
                            r = Err(rpc_error_of(e));
                        }
                    }
                    r
                }
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
            .and_then(|alert| {
                entry
                    .runtime
                    .notify(pid.clone(), alert.clone())
                    .map_err(rpc_error_of)?;
                // Route through the same inbox other producers use; the DB
                // write happens off-loop in `flush_alerts`.
                state.pending_alerts.push((plugin.to_string(), alert));
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
    repaint
}

/// `host.call`: scope-check on the loop, dispatch on the dispatcher thread,
/// answer directly through the plugin's writer.
fn apply_host_call(state: &mut PluginsState, plugin: &str, msg: RpcMessage) -> bool {
    let Some(entry) = state.plugins.get(plugin) else {
        return false;
    };
    let Some(id) = msg.id else {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, "host.call without id ignored");
        return false;
    };
    let Some(writer) = entry.writer.clone() else {
        // One-shot plugins cannot receive replies — skip dispatch entirely.
        tracing::warn!(target: "thegn::plugin", plugin = %plugin, "host.call from a one-shot plugin skipped (no reply channel)");
        return false;
    };
    let Some(cap) = param_str(&msg.params, "cap") else {
        respond(
            entry,
            RpcResponse::err(id, RpcError::new(RpcErrorCode::Invalid, "missing cap")),
        );
        return false;
    };
    if let Err(e) = host_call_check(&entry.plugin.spec, &cap) {
        tracing::debug!(target: "thegn::plugin", plugin = %plugin, cap = %cap, error = %e.message, "host.call denied");
        respond(entry, RpcResponse::err(id, e));
        return false;
    }
    let params = msg
        .params
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    // The event feed is a stream, not a request/response: `events.subscribe`
    // starts an off-loop bridge that forwards feed frames as `on_event`
    // notifications, and the host.call itself is acknowledged immediately.
    if cap == "events.subscribe" {
        spawn_feed_bridge(state.cfg.clone(), writer.clone());
        respond(
            entry,
            RpcResponse::ok(id, serde_json::json!({ "subscribed": true })),
        );
        return false;
    }
    let cfg = state.cfg.clone();
    let dispatcher = state
        .dispatcher
        .get_or_insert_with(|| Dispatcher::spawn(cfg));
    dispatcher.dispatch(writer, id, cap, params);
    false
}

/// Start an off-loop bridge that forwards the daemon control event feed to a
/// resident plugin as `on_event` notifications (the plugin declared a feed
/// subscription via `host.call events.subscribe`, read-scoped). A dedicated
/// thread owns a current-thread runtime + the feed subscription and writes
/// straight to the plugin's [`SessionWriter`] — it never re-enters the
/// compositor loop, so a subscribing plugin costs the idle render loop nothing.
/// The bridge ends when the feed closes or the plugin's session goes away.
fn spawn_feed_bridge(cfg: thegn_core::config::Config, writer: SessionWriter) {
    let spawn = std::thread::Builder::new()
        .name("thegn-plugin-feed".into())
        .spawn(move || {
            // Utility: forwards the control event feed to a plugin whose output the user sees.
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
            });
        });
    if let Err(e) = spawn {
        tracing::warn!(target: "thegn::plugin", error = %e, "feed-bridge thread failed to start");
    }
}

// ---------------------------------------------------------------------------
// host.call dispatcher
// ---------------------------------------------------------------------------

struct DispatchReq {
    writer: SessionWriter,
    id: u64,
    cap: String,
    /// Parameters for the capability (path placeholders + body/query), resolved
    /// against the `API_CALLS` route spine.
    params: serde_json::Value,
}

/// One background thread owning a current-thread tokio runtime + the control
/// client, answering plugins directly through their [`SessionWriter`]s (never
/// re-entering the event loop).
pub(crate) struct Dispatcher {
    tx: std::sync::mpsc::Sender<DispatchReq>,
}

impl Dispatcher {
    fn spawn(cfg: thegn_core::config::Config) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<DispatchReq>();
        let spawn = std::thread::Builder::new()
            .name("thegn-plugin-dispatch".into())
            .spawn(move || {
                // Utility: plugin host.call round-trips back visible plugin content.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!(target: "thegn::plugin", error = %e, "dispatcher runtime failed");
                        return;
                    }
                };
                while let Ok(req) = rx.recv() {
                    let DispatchReq {
                        writer,
                        id,
                        cap,
                        params,
                    } = req;
                    let resp = match rt.block_on(dispatch_one(&cfg, &cap, params)) {
                        Ok(v) => RpcResponse::ok(id, v),
                        Err(e) => RpcResponse::err(id, e),
                    };
                    // best-effort: the session may have died mid-dispatch.
                    let _ = writer.respond(&resp);
                }
            });
        if let Err(e) = spawn {
            tracing::warn!(target: "thegn::plugin", error = %e, "dispatcher thread failed to start");
        }
        Self { tx }
    }

    fn dispatch(&self, writer: SessionWriter, id: u64, cap: String, params: serde_json::Value) {
        // best-effort: a dead dispatcher thread means the daemon reply is lost;
        // the plugin's request times out on its own side.
        // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = self.tx.send(DispatchReq {
            writer,
            id,
            cap,
            params,
        });
    }
}

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
        ApiVersion, CadenceHint, ContributionId, PluginManifest, PluginMode, Span, StyleRole,
    };

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
        PluginSpec {
            manifest: PluginManifest {
                id: PluginId::new(id),
                name: id.into(),
                version: "1".into(),
                api: ApiVersion::new(0, 2, 0),
                capabilities: Vec::new(),
                contributions,
            },
            command: vec!["true".into()],
            cwd: String::new(),
            env: Default::default(),
            timeout_secs: 5,
            scopes,
            mode: PluginMode::Resident,
            enabled: true,
        }
    }

    fn loaded_state(spec: PluginSpec) -> LoadedState {
        let negotiated = thegn_svc::plugin::loader::negotiate(&spec).expect("negotiates");
        LoadedState {
            plugin: LoadedPlugin { spec, dir: None },
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

    fn msg(method: &str, params: serde_json::Value) -> RpcMessage {
        RpcMessage {
            id: None,
            method: method.into(),
            params,
        }
    }

    fn drain_one(state: &mut PluginsState, model: &mut FrameModel, m: PluginMsg) -> bool {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        tx.send(m).unwrap();
        drop(tx);
        drain(&mut rx, state, model, None)
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
        assert!(repaint, "changed view repaints");
        let views = statusbar_views(&state);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].0, "p label");
        assert_eq!(views[0].1.text_content(), "3 mails");
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
                        "view": {"spans": [{"text": "3 mails", "role": "Default"}]}
                    }),
                )),
            },
        );
        assert!(!repaint, "unchanged view is not a repaint");
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
        assert!(drain_one(
            &mut state,
            &mut model,
            PluginMsg::OneShot {
                plugin: "p".into(),
                run: Ok(run)
            }
        ));
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
    fn register_adds_a_contribution_once() {
        let mut state = state_with(vec![seg_spec("p")]);
        let mut model = FrameModel::default();
        let c = contribution(
            "extra",
            ExtensionPoint::StatusBarSegment,
            "p/extra",
            "Extra",
        );
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
        assert_eq!(state.plugins["p"].contributions.len(), 2);
    }

    #[test]
    fn scope_check_denies_dispatches_and_flags_unknown_caps() {
        // No scopes → Denied.
        let bare = seg_spec("p");
        let err = host_call_check(&bare, "sessions.list").unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Denied);
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
    fn palette_actions_list_and_route_to_their_owner() {
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

        // No writer (one-shot) and no host handle: nothing to run, but the
        // key still resolves to its owner (a None host is the only miss).
        assert_eq!(
            invoke_palette_action(&state, None, "plugin:acts:acts.hello"),
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
        assert!(!drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "ghost".into(),
                event: SessionEvent::Message(msg("update", serde_json::json!({}))),
            }
        ));
        assert!(!drain_one(
            &mut state,
            &mut model,
            PluginMsg::Event {
                plugin: "ghost".into(),
                event: SessionEvent::Junk("println! debris".into()),
            }
        ));
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
}
