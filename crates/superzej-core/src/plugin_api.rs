//! The Plugin API Contract (v0)
//!
//! This module defines the transport-agnostic vocabulary of the superzej plugin
//! API. These types are the serialization layer between the host and any plugin
//! mechanism (WASM, subprocess, Rhai).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Display;

/// Semantic version of the API contract itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, schemars::JsonSchema)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// Current Plugin API contract version implemented by this crate.
pub const API_VERSION: ApiVersion = ApiVersion {
    major: 0,
    minor: 1,
    patch: 0,
};

impl ApiVersion {
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl<'de> Deserialize<'de> for ApiVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let mut parts = s.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Ok(Self::new(major, minor, patch))
    }
}

impl Serialize for ApiVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}.{}.{}", self.major, self.minor, self.patch))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct PluginId(String);

impl PluginId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct ContributionId(String);

impl ContributionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct SurfaceId(String);

impl SurfaceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// A capability grant or request (`kind:target`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct Capability(String);

impl Capability {
    pub fn parse(s: &str) -> Option<Self> {
        if s.split_once(':').is_some() {
            Some(Self(s.to_string()))
        } else {
            None
        }
    }

    pub fn new(kind: impl AsRef<str>, target: impl AsRef<str>) -> Self {
        Self(format!("{}:{}", kind.as_ref(), target.as_ref()))
    }

    pub fn kind(&self) -> &str {
        self.0.split_once(':').map(|(kind, _)| kind).unwrap_or("")
    }

    pub fn target(&self) -> &str {
        self.0
            .split_once(':')
            .map(|(_, target)| target)
            .unwrap_or("")
    }
}

fn surface_capability_for(ep: &ExtensionPoint) -> Option<Capability> {
    let scope = match ep {
        ExtensionPoint::StatusBarSegment => "statusbar",
        ExtensionPoint::SidebarTab => "sidebar",
        ExtensionPoint::PaletteAction => "palette",
        ExtensionPoint::NotificationSource => "notification",
        ExtensionPoint::HarnessAdapter => "harness",
        ExtensionPoint::ProgramAdapter => "program",
        ExtensionPoint::Theme => "theme",
        ExtensionPoint::Automation => "automation",
        ExtensionPoint::DataSource => "data",
        ExtensionPoint::Unknown(_) => return None,
    };
    Some(Capability::new("surface", scope))
}

/// The typed slots the host offers for plugins to fill.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, schemars::JsonSchema)]
pub enum ExtensionPoint {
    StatusBarSegment,
    SidebarTab,
    PaletteAction,
    NotificationSource,
    HarnessAdapter,
    ProgramAdapter,
    Theme,
    Automation,
    DataSource,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CadenceHint {
    OnDemand,
    Interval { millis: u64 },
    OnEvent { events: Vec<String> },
}

/// A plugin's request to claim a single ExtensionPoint instance.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Contribution {
    pub id: ContributionId,
    pub extension_point: ExtensionPoint,
    pub label: String,
    pub surface: Option<SurfaceId>,
    #[serde(default = "default_on_demand")]
    pub cadence: CadenceHint,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

fn default_on_demand() -> CadenceHint {
    CadenceHint::OnDemand
}

/// The plugin's identity and its full capability/contribution declaration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PluginManifest {
    pub id: PluginId,
    pub name: String,
    pub version: String,
    pub api: ApiVersion,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub contributions: Vec<Contribution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginApiError {
    IncompatibleApi {
        required: ApiVersion,
        got: ApiVersion,
    },
    CapabilityDenied {
        capability: Capability,
        operation: String,
    },
    UnknownExtensionPoint(String),
}

impl Display for PluginApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleApi { required, got } => {
                write!(f, "incompatible api: host {required:?}, plugin {got:?}")
            }
            Self::CapabilityDenied {
                capability,
                operation,
            } => {
                write!(
                    f,
                    "denied: capability {:?} required for {}",
                    capability.0, operation
                )
            }
            Self::UnknownExtensionPoint(s) => write!(f, "unknown extension point: {s}"),
        }
    }
}

/// A negotiated load session: the host's answer to the manifest.
#[derive(Debug, Clone, Default)]
pub struct NegotiatedManifest {
    pub api: ApiVersion,
    pub granted: std::collections::HashSet<Capability>,
    pub denied: std::collections::HashSet<Capability>,
    pub accepted_contributions: Vec<Contribution>,
    pub unsupported_contributions: Vec<Contribution>,
}

impl NegotiatedManifest {
    pub fn is_capability_granted(&self, cap: &Capability) -> bool {
        self.granted.contains(cap)
    }

    pub fn is_capability_denied(&self, cap: &Capability) -> bool {
        self.denied.contains(cap)
    }
}

pub struct HostContract {
    pub api_version: ApiVersion,
    pub available_extension_points: std::collections::HashSet<ExtensionPoint>,
    pub granted_capabilities: std::collections::HashSet<Capability>,
}

impl HostContract {
    pub fn new(api: ApiVersion) -> Self {
        Self {
            api_version: api,
            available_extension_points: Default::default(),
            granted_capabilities: Default::default(),
        }
    }

    pub fn with_extension_points(mut self, eps: impl IntoIterator<Item = ExtensionPoint>) -> Self {
        self.available_extension_points.extend(eps);
        self
    }

    pub fn with_grants(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.granted_capabilities.extend(caps);
        self
    }

    pub fn negotiate(
        &self,
        manifest: &PluginManifest,
    ) -> Result<NegotiatedManifest, PluginApiError> {
        if manifest.api.major != self.api_version.major
            || manifest.api.minor > self.api_version.minor
        {
            return Err(PluginApiError::IncompatibleApi {
                required: self.api_version,
                got: manifest.api,
            });
        }

        let mut neg = NegotiatedManifest {
            api: manifest.api,
            ..Default::default()
        };

        for cap in &manifest.capabilities {
            if self.granted_capabilities.contains(cap) {
                neg.granted.insert(cap.clone());
            } else {
                neg.denied.insert(cap.clone());
            }
        }

        for contrib in &manifest.contributions {
            if self
                .available_extension_points
                .contains(&contrib.extension_point)
            {
                neg.accepted_contributions.push(contrib.clone());
            } else {
                neg.unsupported_contributions.push(contrib.clone());
            }
        }

        Ok(neg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditDecision {
    Granted,
    Denied,
}

#[derive(Debug, Clone)]
pub struct AuditLogEntry {
    pub plugin: PluginId,
    pub capability: Capability,
    pub operation: String,
    pub decision: AuditDecision,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub enum IoStatus {
    Accepted,
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct IoResult {
    pub status: IoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct IoRequest {
    pub scheme: String,
    pub target: String,
    pub payload: serde_json::Value,
}

impl IoRequest {
    pub fn network(method: &str, url: &str) -> Self {
        Self {
            scheme: "network".into(),
            target: url.into(),
            payload: serde_json::json!({ "method": method }),
        }
    }

    pub fn run(cmd: &str, args: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        Self {
            scheme: "run".into(),
            target: cmd.into(),
            payload: serde_json::json!({ "args": args }),
        }
    }

    pub fn required_capability(&self) -> Capability {
        match self.scheme.as_str() {
            "network" => Capability::new("network", host_from_url(&self.target)),
            "run" => Capability::new("run", &self.target),
            other => Capability::new(other, &self.target),
        }
    }
}

fn host_from_url(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
        .split('@')
        .next_back()
        .unwrap_or(after_scheme)
        .split(':')
        .next()
        .unwrap_or(after_scheme)
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Alert {
    pub source: String,
    pub message: String,
}

impl Alert {
    pub fn new(source: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            message: msg.into(),
        }
    }
}

pub struct PluginRuntime {
    manifest: NegotiatedManifest,
    audit: Vec<AuditLogEntry>,
    state: BTreeMap<String, serde_json::Value>,
    host_values: BTreeMap<String, serde_json::Value>,
    subscriptions: std::collections::HashSet<(PluginId, EventKind)>,
    events: Vec<Event>,
    surface_caps: BTreeMap<SurfaceId, Capability>,
    views: SurfaceCache,
}

impl PluginRuntime {
    pub fn new(manifest: NegotiatedManifest) -> Self {
        let surface_caps = manifest
            .accepted_contributions
            .iter()
            .filter_map(|c| {
                c.surface
                    .clone()
                    .zip(surface_capability_for(&c.extension_point))
            })
            .collect();
        Self {
            manifest,
            audit: Vec::new(),
            state: Default::default(),
            host_values: Default::default(),
            subscriptions: Default::default(),
            events: Default::default(),
            surface_caps,
            views: SurfaceCache::default(),
        }
    }

    pub fn with_host_value(mut self, key: &str, val: serde_json::Value) -> Self {
        self.host_values.insert(key.to_string(), val);
        self
    }

    pub fn register(
        &mut self,
        plugin: PluginId,
        contribution: Contribution,
    ) -> Result<(), PluginApiError> {
        if let Some(cap) = surface_capability_for(&contribution.extension_point) {
            self.audit(plugin, cap, "register")?;
        }
        if let Some(surface) = contribution.surface {
            if let Some(cap) = surface_capability_for(&contribution.extension_point) {
                self.surface_caps.insert(surface, cap);
            }
        }
        Ok(())
    }

    pub fn subscribe(&mut self, plugin: PluginId, kind: EventKind) -> Result<(), PluginApiError> {
        self.subscriptions.insert((plugin, kind));
        Ok(())
    }

    pub fn subscriptions(&self) -> &std::collections::HashSet<(PluginId, EventKind)> {
        &self.subscriptions
    }

    pub fn update(
        &mut self,
        plugin: PluginId,
        surface: SurfaceId,
        view: View,
    ) -> Result<UpdateResult, PluginApiError> {
        let cap = self
            .surface_caps
            .get(&surface)
            .cloned()
            .unwrap_or_else(|| Capability::new("surface", "unknown"));
        self.audit(plugin, cap, "update")?;
        Ok(self.views.update(surface, view))
    }

    pub fn invalidate(
        &mut self,
        plugin: PluginId,
        surface: SurfaceId,
    ) -> Result<(), PluginApiError> {
        let cap = self
            .surface_caps
            .get(&surface)
            .cloned()
            .unwrap_or_else(|| Capability::new("surface", "unknown"));
        self.audit(plugin, cap, "invalidate")?;
        self.views.invalidate(&surface);
        Ok(())
    }

    pub fn view(&self, surface: &SurfaceId) -> Option<&View> {
        self.views.view(surface)
    }

    pub fn is_dirty(&self, surface: &SurfaceId) -> bool {
        self.views.is_dirty(surface)
    }

    pub fn emit(&mut self, _plugin: PluginId, event: Event) -> Result<(), PluginApiError> {
        self.events.push(event);
        Ok(())
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn host_value(
        &self,
        _plugin: PluginId,
        key: &str,
    ) -> Result<Option<serde_json::Value>, PluginApiError> {
        Ok(self.host_values.get(key).cloned())
    }

    fn audit(
        &mut self,
        plugin: PluginId,
        capability: Capability,
        operation: &str,
    ) -> Result<(), PluginApiError> {
        if self.manifest.granted.contains(&capability) {
            self.audit.push(AuditLogEntry {
                plugin,
                capability,
                operation: operation.to_string(),
                decision: AuditDecision::Granted,
                timestamp_ms: 0,
            });
            Ok(())
        } else {
            self.audit.push(AuditLogEntry {
                plugin: plugin.clone(),
                capability: capability.clone(),
                operation: operation.to_string(),
                decision: AuditDecision::Denied,
                timestamp_ms: 0,
            });
            Err(PluginApiError::CapabilityDenied {
                capability,
                operation: operation.to_string(),
            })
        }
    }

    pub fn io(&mut self, plugin: PluginId, req: IoRequest) -> Result<IoResult, PluginApiError> {
        let cap = req.required_capability();
        self.audit(plugin, cap, &format!("io.{}", req.scheme))?;
        Ok(IoResult {
            status: IoStatus::Accepted,
            body: None,
        })
    }

    pub fn notify(&mut self, plugin: PluginId, alert: Alert) -> Result<(), PluginApiError> {
        let cap = Capability::parse(&format!("notify:{}", alert.source))
            .unwrap_or_else(|| Capability("unknown".into()));
        self.audit(plugin, cap, "notify")?;
        Ok(())
    }

    pub fn state_set(
        &mut self,
        plugin: PluginId,
        key: &str,
        val: serde_json::Value,
    ) -> Result<(), PluginApiError> {
        let state_key = format!("{}:{key}", plugin.as_str());
        let cap = Capability::parse(&format!("state:{}", plugin.as_str())).unwrap();
        self.audit(plugin, cap, "state.set")?;
        self.state.insert(state_key, val);
        Ok(())
    }

    pub fn state_get(
        &mut self,
        plugin: PluginId,
        key: &str,
    ) -> Result<Option<serde_json::Value>, PluginApiError> {
        let state_key = format!("{}:{key}", plugin.as_str());
        let cap = Capability::parse(&format!("state:{}", plugin.as_str())).unwrap();
        self.audit(plugin, cap, "state.get")?;
        Ok(self.state.get(&state_key).cloned())
    }

    pub fn audit_log(&self) -> &[AuditLogEntry] {
        &self.audit
    }
}

// ----------------------------------------------------------------------------
// Render model
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub enum StyleRole {
    Default,
    Accent,
    Warning,
    Error,
    Faint,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Span {
    pub text: String,
    pub role: StyleRole,
}

impl Span {
    pub fn styled(text: impl Into<String>, role: StyleRole) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct View {
    pub spans: Vec<Span>,
}

impl View {
    pub fn line(spans: impl IntoIterator<Item = Span>) -> Self {
        Self {
            spans: spans.into_iter().collect(),
        }
    }

    pub fn text_content(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

#[derive(Debug, Clone)]
pub struct CachedView {
    pub view: View,
    pub degraded: bool,
}

impl CachedView {
    pub fn text_content(&self) -> String {
        self.view.text_content()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DegradeReason {
    RenderBudgetExceeded,
    Crash,
}

pub struct UpdateResult {
    pub changed: bool,
}

#[derive(Default)]
pub struct SurfaceCache {
    surfaces: BTreeMap<SurfaceId, (View, bool)>,
}

impl SurfaceCache {
    pub fn update(&mut self, surface: SurfaceId, view: View) -> UpdateResult {
        let changed = self
            .surfaces
            .get(&surface)
            .map(|(v, _)| v != &view)
            .unwrap_or(true);
        self.surfaces.insert(surface, (view, false));
        UpdateResult { changed }
    }

    pub fn invalidate(&mut self, surface: &SurfaceId) {
        if let Some((_, dirty)) = self.surfaces.get_mut(surface) {
            *dirty = true;
        }
    }

    pub fn is_dirty(&self, surface: &SurfaceId) -> bool {
        self.surfaces
            .get(surface)
            .map(|(_, dirty)| *dirty)
            .unwrap_or(true)
    }

    pub fn view(&self, surface: &SurfaceId) -> Option<&View> {
        self.surfaces.get(surface).map(|(v, _)| v)
    }

    pub fn degrade(&mut self, surface: &SurfaceId, _reason: DegradeReason) -> CachedView {
        let view = if let Some((v, _)) = self.surfaces.get(surface) {
            let mut degraded_view = v.clone();
            degraded_view
                .spans
                .push(Span::styled(" ⚠", StyleRole::Warning));
            degraded_view
        } else {
            View::line([Span::styled("⚠", StyleRole::Warning)])
        };

        CachedView {
            view,
            degraded: true,
        }
    }
}

// ----------------------------------------------------------------------------
// Transport
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostVerb {
    Register,
    Subscribe,
    Update,
    Invalidate,
    Io,
    Notify,
    Emit,
    StateGet,
    StateSet,
    HostValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginCallback {
    Activate,
    OnEvent,
    Render,
    Deactivate,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, schemars::JsonSchema)]
pub enum EventKind {
    Timer,
    FocusChanged,
    FileChanged,
    BusMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Event {
    pub kind: EventKind,
    pub payload: serde_json::Value,
}

impl Event {
    pub fn new(kind: EventKind, payload: serde_json::Value) -> Self {
        Self { kind, payload }
    }
}

/// JSON-RPC projection
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RpcMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    pub params: serde_json::Value,
}

impl RpcMessage {
    pub fn request(id: u64, method: HostVerb, params: serde_json::Value) -> Self {
        Self {
            id: Some(id),
            method: method.method_name().to_string(),
            params,
        }
    }

    pub fn notification(method: PluginCallback, params: serde_json::Value) -> Self {
        Self {
            id: None,
            method: method.method_name().to_string(),
            params,
        }
    }

    pub fn method(&self) -> Option<&str> {
        Some(&self.method)
    }
}

impl HostVerb {
    pub fn method_name(self) -> &'static str {
        match self {
            HostVerb::Register => "register",
            HostVerb::Subscribe => "subscribe",
            HostVerb::Update => "update",
            HostVerb::Invalidate => "invalidate",
            HostVerb::Io => "io",
            HostVerb::Notify => "notify",
            HostVerb::Emit => "emit",
            HostVerb::StateGet => "state.get",
            HostVerb::StateSet => "state.set",
            HostVerb::HostValue => "host.value",
        }
    }
}

impl PluginCallback {
    pub fn method_name(self) -> &'static str {
        match self {
            PluginCallback::Activate => "activate",
            PluginCallback::OnEvent => "on_event",
            PluginCallback::Render => "render",
            PluginCallback::Deactivate => "deactivate",
        }
    }
}
