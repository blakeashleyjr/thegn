//! The Plugin API Contract (v0)
//!
//! This module defines the transport-agnostic vocabulary of the thegn plugin
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
    minor: 2,
    patch: 0,
};

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

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

    /// The whole `"kind:target"` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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
    /// Provider contributions (CI / issue / forge) declare the seam's caps
    /// struct here; the host deserializes it into the seam's `XCaps` at load
    /// (missing keys ⇒ `false`, least privilege). `Null` for non-providers.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub caps: serde_json::Value,
    /// `PaletteAction` contributions may ask for a default chord (the user's
    /// `[keybinds]` still wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chord: Option<String>,
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

/// How the host runs a plugin process.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginMode {
    /// Spawn per poll/render, read NDJSON to exit (the calendar `command`
    /// source's shape). Stdin is closed.
    #[default]
    OneShot,
    /// One long-lived process driven over stdin/stdout for the session.
    Resident,
}

fn default_timeout_secs() -> u64 {
    30
}
fn default_true() -> bool {
    true
}

/// Everything needed to *run* a plugin: the manifest says what it is, the
/// spec says how to start it. This is the `[[plugins]]` config shape (and a
/// `plugin.toml` in a plugin directory); every field beyond the manifest has
/// a default, so a v0.1 manifest plus `command` is a valid v0.2 spec.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PluginSpec {
    #[serde(flatten)]
    pub manifest: PluginManifest,
    /// argv — never a shell string, so arguments with spaces survive and no
    /// shell is involved in launching plugin code.
    pub command: Vec<String>,
    /// Working directory; empty = the plugin's own directory (or the host cwd
    /// for config-declared plugins).
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Per-call (one-shot: per-run) wall-clock cap before the process group
    /// is killed.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Host-capability scopes this plugin holds for `host.call` — the same
    /// lattice as control-API tokens (`read` / `write` / `git` / `admin`), so
    /// a plugin is authorised exactly like a paired phone.
    #[serde(default)]
    pub scopes: Vec<crate::control::Scope>,
    #[serde(default)]
    pub mode: PluginMode,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl PluginSpec {
    /// The scope set a `host.call` is checked against.
    pub fn scope_set(&self) -> crate::control::ScopeSet {
        crate::control::ScopeSet::of(&self.scopes)
    }
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
        if let Some(surface) = contribution.surface
            && let Some(cap) = surface_capability_for(&contribution.extension_point)
        {
            self.surface_caps.insert(surface, cap);
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
    /// Invoke a host capability by catalog id (`{"cap": "sessions.list",
    /// "params": {…}}`), checked against the plugin's scope set exactly as a
    /// control-API token would be. A request: carries an `id` and gets a
    /// [`RpcResponse`].
    HostCall,
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
    /// A `PaletteAction` contribution was invoked (`payload.id`).
    Action,
    /// The active worktree changed (`payload.path`, `payload.branch`).
    WorktreeChanged,
    /// A session's process exited (`payload.session`, `payload.code`).
    SessionExit,
    /// A notification was raised (`payload` = the notification).
    Notification,
    /// Anything else: a newer host's event a v0.2 plugin does not know, kept
    /// rather than dropped so it can still be logged or ignored by name.
    #[serde(untagged)]
    Custom(String),
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
    /// Defaulted so a verb that carries no arguments can be written as a bare
    /// `{"method":"..."}` — a plugin author shouldn't have to type `"params":{}`.
    #[serde(default)]
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
            HostVerb::HostCall => "host.call",
        }
    }

    /// Every verb (for wire tests and the plugin-surface coverage table).
    pub const ALL: &'static [HostVerb] = &[
        HostVerb::Register,
        HostVerb::Subscribe,
        HostVerb::Update,
        HostVerb::Invalidate,
        HostVerb::Io,
        HostVerb::Notify,
        HostVerb::Emit,
        HostVerb::StateGet,
        HostVerb::StateSet,
        HostVerb::HostValue,
        HostVerb::HostCall,
    ];
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

// ----------------------------------------------------------------------------
// Replies (v0.2)
// ----------------------------------------------------------------------------

/// Why a request failed, in a vocabulary both directions share. Provider
/// plugins map these onto the seam's [`ErrorClass`](crate::seam::ErrorClass)
/// (`unsupported` ⇒ the same value a defaulted optional op returns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    Unsupported,
    NotFound,
    Denied,
    Auth,
    RateLimited,
    Timeout,
    Invalid,
    Other,
}

impl RpcErrorCode {
    /// The seam classification of this code.
    pub fn class(self) -> crate::seam::ErrorClass {
        use crate::seam::ErrorClass as C;
        match self {
            RpcErrorCode::Unsupported => C::Unsupported,
            RpcErrorCode::NotFound => C::NotFound,
            RpcErrorCode::Denied | RpcErrorCode::Auth => C::Auth,
            RpcErrorCode::RateLimited => C::RateLimited,
            RpcErrorCode::Timeout => C::Transient,
            RpcErrorCode::Invalid | RpcErrorCode::Other => C::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

impl RpcError {
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: serde_json::Value::Null,
        }
    }
}

/// A reply to an `id`-bearing [`RpcMessage`], either direction. Exactly one
/// of `result` / `error` is set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RpcResponse {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        RpcResponse {
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: u64, error: RpcError) -> Self {
        RpcResponse {
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// One NDJSON line, either direction. A line with `method` is a message
/// (request when it carries `id`, notification otherwise); a line with
/// `result` or `error` is a response. A bare `{"method": …}` still decodes
/// as a v0.1 message.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Frame {
    Message(RpcMessage),
    Response(RpcResponse),
}

impl Frame {
    /// Decode one NDJSON line.
    pub fn parse_line(line: &str) -> Result<Frame, serde_json::Error> {
        serde_json::from_str(line)
    }
}

impl PartialEq for RpcMessage {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.method == other.method && self.params == other.params
    }
}

/// Host capabilities reachable from plugins via `host.call`, by catalog id.
/// Empty until the plugin runtime lands; every `Surface::Plugin` row is
/// excused in `SURFACE_GAPS` until then, and this table is what retires
/// those excuses.
pub const PLUGIN_HOST_CALL_CAPS: &[&str] = &[];

#[cfg(test)]
mod wire_tests {
    use super::*;

    #[test]
    fn response_line_decodes() {
        let f = Frame::parse_line(r#"{"id":7,"result":{"ok":true}}"#).unwrap();
        match f {
            Frame::Response(r) => {
                assert_eq!(r.id, 7);
                assert_eq!(r.result, Some(serde_json::json!({"ok": true})));
                assert!(r.error.is_none());
            }
            other => panic!("{other:?}"),
        }
        let f =
            Frame::parse_line(r#"{"id":8,"error":{"code":"unsupported","message":"no ci.logs"}}"#)
                .unwrap();
        match f {
            Frame::Response(r) => {
                let e = r.error.unwrap();
                assert_eq!(e.code, RpcErrorCode::Unsupported);
                assert_eq!(e.code.class(), crate::seam::ErrorClass::Unsupported);
                assert_eq!(e.data, serde_json::Value::Null);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn legacy_message_line_decodes() {
        let f = Frame::parse_line(r#"{"method":"manifest"}"#).unwrap();
        match f {
            Frame::Message(m) => {
                assert_eq!(m.method, "manifest");
                assert!(m.id.is_none());
                assert_eq!(m.params, serde_json::Value::Null);
            }
            other => panic!("{other:?}"),
        }
        // A request (with id) is still a message, not a response.
        let f = Frame::parse_line(r#"{"id":1,"method":"state.get","params":{"key":"k"}}"#).unwrap();
        assert!(matches!(f, Frame::Message(ref m) if m.id == Some(1)));
        assert!(Frame::parse_line("not json").is_err());
    }

    #[test]
    fn responses_serialize_minimally_and_round_trip() {
        let ok = RpcResponse::ok(1, serde_json::json!([1, 2]));
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            r#"{"id":1,"result":[1,2]}"#
        );
        let err = RpcResponse::err(2, RpcError::new(RpcErrorCode::Denied, "no scope"));
        let j = serde_json::to_string(&err).unwrap();
        assert_eq!(
            j,
            r#"{"id":2,"error":{"code":"denied","message":"no scope"}}"#
        );
        let back = Frame::parse_line(&j).unwrap();
        assert_eq!(back, Frame::Response(err));
        let msg = RpcMessage::request(3, HostVerb::HostCall, serde_json::json!({"cap": "me"}));
        let j = serde_json::to_string(&msg).unwrap();
        assert!(j.contains(r#""method":"host.call""#));
        assert_eq!(Frame::parse_line(&j).unwrap(), Frame::Message(msg));
    }

    #[test]
    fn every_error_code_classifies() {
        for c in [
            RpcErrorCode::Unsupported,
            RpcErrorCode::NotFound,
            RpcErrorCode::Denied,
            RpcErrorCode::Auth,
            RpcErrorCode::RateLimited,
            RpcErrorCode::Timeout,
            RpcErrorCode::Invalid,
            RpcErrorCode::Other,
        ] {
            let _ = c.class();
            let j = serde_json::to_string(&c).unwrap();
            let back: RpcErrorCode = serde_json::from_str(&j).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn minimal_plugin_spec_parses_with_defaults() {
        let spec: PluginSpec = toml::from_str(
            r#"
id = "hello"
name = "Hello"
version = "0.1.0"
api = "0.2.0"
command = ["sh", "hello.sh"]
"#,
        )
        .unwrap();
        assert_eq!(spec.manifest.id.as_str(), "hello");
        assert_eq!(spec.command, ["sh", "hello.sh"]);
        assert_eq!(spec.mode, PluginMode::OneShot);
        assert!(spec.enabled);
        assert!(spec.scopes.is_empty());
        assert_eq!(spec.timeout_secs, 30);
        assert!(spec.cwd.is_empty() && spec.env.is_empty());
        assert!(!spec.scope_set().allows(crate::control::Scope::Read));
        // Round trip keeps the flattened manifest fields at the top level.
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["id"], "hello");
        assert_eq!(v["command"][0], "sh");
        let back: PluginSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn plugin_spec_scopes_use_the_token_lattice() {
        let spec: PluginSpec = toml::from_str(
            r#"
id = "p"
name = "P"
version = "1"
api = "0.2.0"
command = ["p"]
scopes = ["read", "git"]
mode = "resident"
timeout_secs = 5
"#,
        )
        .unwrap();
        let set = spec.scope_set();
        assert!(set.allows(crate::control::Scope::Read));
        assert!(set.allows(crate::control::Scope::Git));
        assert!(!set.allows(crate::control::Scope::Write));
        assert_eq!(spec.mode, PluginMode::Resident);
        assert_eq!(spec.timeout_secs, 5);
    }

    #[test]
    fn unknown_extension_point_still_negotiates_only_that_contribution() {
        let m: PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "x", "name": "X", "version": "1", "api": "0.2.0",
            "contributions": [
                {"id": "a", "extension_point": "StatusBarSegment", "label": "A"},
                {"id": "b", "extension_point": "HologramTab", "label": "B"}
            ]
        }))
        .unwrap();
        assert_eq!(
            m.contributions[1].extension_point,
            ExtensionPoint::Unknown("HologramTab".into())
        );
        assert_eq!(m.contributions[1].caps, serde_json::Value::Null);
        assert!(m.contributions[1].chord.is_none());
        let host = HostContract {
            api_version: API_VERSION,
            available_extension_points: [ExtensionPoint::StatusBarSegment].into_iter().collect(),
            granted_capabilities: Default::default(),
        };
        let neg = host.negotiate(&m).unwrap();
        assert_eq!(neg.accepted_contributions.len(), 1);
        assert_eq!(neg.unsupported_contributions.len(), 1);
        assert_eq!(neg.unsupported_contributions[0].id.as_str(), "b");
    }

    #[test]
    fn event_kinds_keep_unknown_names() {
        let k: EventKind = serde_json::from_str(r#""Timer""#).unwrap();
        assert_eq!(k, EventKind::Timer);
        let k: EventKind = serde_json::from_str(r#""SomethingNew""#).unwrap();
        assert_eq!(k, EventKind::Custom("SomethingNew".into()));
        assert_eq!(
            serde_json::to_string(&EventKind::Action).unwrap(),
            r#""Action""#
        );
    }

    #[test]
    fn host_verb_method_names_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for v in HostVerb::ALL {
            assert!(seen.insert(v.method_name()), "duplicate {:?}", v);
        }
        assert_eq!(HostVerb::HostCall.method_name(), "host.call");
        assert_eq!(HostVerb::ALL.len(), 11);
    }

    #[test]
    fn plugin_host_calls_cover_catalog() {
        let problems = crate::capability::coverage_problems(
            crate::capability::Surface::Plugin,
            PLUGIN_HOST_CALL_CAPS,
        );
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
