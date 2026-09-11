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
///
/// v0.3 (additive over v0.2): [`View`] gains optional multi-row `rows`, [`Span`]
/// gains an optional theme-slot `slot` (with [`StyleRole`] as the fallback), and
/// [`ExtensionPoint::PanelSection`] joins the vocabulary. Every addition
/// defaults, so a v0.2 plugin and an older host keep working (the negotiation in
/// [`HostContract::negotiate`] accepts a lower-or-equal minor).
///
/// 0.2 → 0.3: additive — the control `Scope` lattice gained `exec` (the
/// `[[presets]]` launch capability), which widens the scope enum the plugin
/// manifest projects. Older plugins keep negotiating; nothing was removed.
pub const API_VERSION: ApiVersion = ApiVersion {
    major: 0,
    minor: 3,
    patch: 0,
};

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl ApiVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
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
        let parse_part = |part: Option<&str>| {
            part.and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| {
                    <D::Error as serde::de::Error>::custom(
                        "plugin API version must be major.minor.patch with numeric components",
                    )
                })
        };
        let major = parse_part(parts.next())?;
        let minor = parse_part(parts.next())?;
        let patch = parse_part(parts.next())?;
        if parts.next().is_some() {
            return Err(<D::Error as serde::de::Error>::custom(
                "plugin API version must contain exactly three numeric components",
            ));
        }
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
        ExtensionPoint::PanelSection => "panel",
        ExtensionPoint::SidebarTab => "sidebar",
        ExtensionPoint::PaletteAction => "palette",
        ExtensionPoint::NotificationSource => "notification",
        ExtensionPoint::IssueProvider
        | ExtensionPoint::CiProvider
        | ExtensionPoint::ForgeProvider => "provider",
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
    /// Reserved v0.3 vocabulary for a plugin-contributed accordion section in
    /// the info panel. The wire shape and cached [`View`] model are stable, but
    /// current hosts negotiate this point as unsupported until host-side
    /// accordion placement and row activation land.
    PanelSection,
    SidebarTab,
    PaletteAction,
    NotificationSource,
    HarnessAdapter,
    ProgramAdapter,
    Theme,
    Automation,
    DataSource,
    /// The plugin IS an issue-tracker backend: the host bridges the issue
    /// seam's operations to it as `provider.call` requests (`seam:
    /// "issues"`). The contribution's `caps` may carry provider facts; its
    /// `label` is the account name shown in the panel.
    IssueProvider,
    /// Reserved vocabulary: a plugin-backed CI provider. Accepted by the
    /// wire, negotiated unsupported by the host until CI selection can name
    /// plugin providers.
    CiProvider,
    /// Reserved vocabulary: a plugin-backed forge. Same status as
    /// [`ExtensionPoint::CiProvider`].
    ForgeProvider,
    #[serde(untagged)]
    Unknown(String),
}

impl ExtensionPoint {
    /// Stable wire spelling used by contract inspection and diagnostics.
    pub fn wire_name(&self) -> &str {
        match self {
            Self::StatusBarSegment => "StatusBarSegment",
            Self::PanelSection => "PanelSection",
            Self::SidebarTab => "SidebarTab",
            Self::PaletteAction => "PaletteAction",
            Self::NotificationSource => "NotificationSource",
            Self::HarnessAdapter => "HarnessAdapter",
            Self::ProgramAdapter => "ProgramAdapter",
            Self::Theme => "Theme",
            Self::Automation => "Automation",
            Self::DataSource => "DataSource",
            Self::IssueProvider => "IssueProvider",
            Self::CiProvider => "CiProvider",
            Self::ForgeProvider => "ForgeProvider",
            Self::Unknown(name) => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CadenceHint {
    OnDemand,
    Interval { millis: u64 },
    OnEvent { events: Vec<String> },
}

impl CadenceHint {
    pub fn kind(&self) -> CadenceKind {
        match self {
            Self::OnDemand => CadenceKind::OnDemand,
            Self::Interval { .. } => CadenceKind::Interval,
            Self::OnEvent { .. } => CadenceKind::OnEvent,
        }
    }
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

impl PluginMode {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::OneShot => "one_shot",
            Self::Resident => "resident",
        }
    }
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
    /// lattice as control-API tokens (`read` / `write` / `git` / `exec` /
    /// `admin`), so a plugin is authorised exactly like a paired client.
    /// Write, Git, and Exec are independent; Admin implies all of them.
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
    UnsupportedExtensionPoint(String),
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
            Self::UnsupportedExtensionPoint(s) => {
                write!(f, "unsupported extension point: {s}")
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
    /// Compatibility projection retained for callers that only distinguish
    /// accepted from unsupported contributions. New inspection surfaces use
    /// [`NegotiatedManifest::rejected_contributions`] for stable reasons.
    pub unsupported_contributions: Vec<Contribution>,
    pub rejected_contributions: Vec<ContributionRejection>,
    /// Every point this host contract will permit for runtime `register`.
    pub supported_extension_points: std::collections::HashSet<ExtensionPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributionRejection {
    pub contribution: Contribution,
    pub reason: String,
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
    extension_support: Vec<ExtensionPointSupport>,
}

impl HostContract {
    pub fn new(api: ApiVersion) -> Self {
        Self {
            api_version: api,
            available_extension_points: Default::default(),
            granted_capabilities: Default::default(),
            extension_support: Vec::new(),
        }
    }

    pub fn with_extension_points(mut self, eps: impl IntoIterator<Item = ExtensionPoint>) -> Self {
        self.available_extension_points.extend(eps);
        self
    }

    /// Install the complete support table for one host build. Only `wired`
    /// rows become negotiable; reserved and separately-owned vocabulary stays
    /// inspectable without accidentally becoming runtime support.
    pub fn with_extension_support(
        mut self,
        rows: impl IntoIterator<Item = ExtensionPointSupport>,
    ) -> Self {
        self.extension_support.extend(rows);
        self.available_extension_points.extend(
            self.extension_support
                .iter()
                .filter(|row| row.state == SupportState::Wired)
                .map(|row| row.extension_point.clone()),
        );
        self
    }

    pub fn with_grants(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.granted_capabilities.extend(caps);
        self
    }

    pub fn extension_support(&self) -> &[ExtensionPointSupport] {
        &self.extension_support
    }

    pub fn support_for(&self, point: &ExtensionPoint) -> Option<&ExtensionPointSupport> {
        self.extension_support
            .iter()
            .find(|row| &row.extension_point == point)
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
            supported_extension_points: self.available_extension_points.clone(),
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
            let reason = if !self
                .available_extension_points
                .contains(&contrib.extension_point)
            {
                self.support_for(&contrib.extension_point)
                    .map(ExtensionPointSupport::unavailable_reason)
                    .unwrap_or_else(|| {
                        format!(
                            "unsupported extension point {}: not present in this host contract",
                            contrib.extension_point.wire_name()
                        )
                    })
                    .into()
            } else if let Some(required) = self
                .support_for(&contrib.extension_point)
                .and_then(|row| row.required_capability)
                && !manifest
                    .capabilities
                    .iter()
                    .any(|cap| cap.as_str() == required)
            {
                Some(format!(
                    "extension point {} requires declared capability {required}",
                    contrib.extension_point.wire_name()
                ))
            } else {
                None
            };
            if let Some(reason) = reason {
                neg.unsupported_contributions.push(contrib.clone());
                neg.rejected_contributions.push(ContributionRejection {
                    contribution: contrib.clone(),
                    reason,
                });
            } else {
                neg.accepted_contributions.push(contrib.clone());
            }
        }

        Ok(neg)
    }

    /// Negotiate a runnable plugin spec, adding mode and cadence checks that
    /// cannot be decided from a manifest alone.
    pub fn negotiate_spec(&self, spec: &PluginSpec) -> Result<NegotiatedManifest, PluginApiError> {
        let mut neg = self.negotiate(&spec.manifest)?;
        // Runtime `register` may repeat or add a contribution after startup.
        // Keep that path on the same mode boundary as manifest negotiation.
        neg.supported_extension_points.retain(|point| {
            self.support_for(point).is_some_and(|row| {
                row.state == SupportState::Wired && row.modes.contains(&spec.mode)
            })
        });
        let accepted = std::mem::take(&mut neg.accepted_contributions);
        for contribution in accepted {
            let rejection = self.support_for(&contribution.extension_point).and_then(|row| {
                if !row.modes.contains(&spec.mode) {
                    Some(format!(
                        "extension point {} does not support mode {}; supported modes: {}",
                        contribution.extension_point.wire_name(),
                        spec.mode.wire_name(),
                        row.mode_names().join(",")
                    ))
                } else if !row.cadences.contains(&contribution.cadence.kind()) {
                    Some(format!(
                        "extension point {} does not support cadence {}; supported cadences: {}",
                        contribution.extension_point.wire_name(),
                        contribution.cadence.kind().as_str(),
                        row.cadence_names().join(",")
                    ))
                } else {
                    None
                }
            });
            if let Some(reason) = rejection {
                neg.unsupported_contributions.push(contribution.clone());
                neg.rejected_contributions.push(ContributionRejection {
                    contribution,
                    reason,
                });
            } else {
                neg.accepted_contributions.push(contribution);
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
        if !self
            .manifest
            .supported_extension_points
            .contains(&contribution.extension_point)
        {
            return Err(PluginApiError::UnsupportedExtensionPoint(
                contribution.extension_point.wire_name().to_string(),
            ));
        }
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

    /// Record the independent control-scope decision for a `host.call` before
    /// the request crosses onto the generic control dispatcher. Host calls do
    /// not use manifest `Capability` grants, so they cannot go through
    /// the runtime's private capability-audit helper; `host:<catalog-id>` keeps
    /// these decisions in the same audit stream as surface and I/O grants.
    pub fn record_host_call_decision(
        &mut self,
        plugin: PluginId,
        capability_id: &str,
        decision: AuditDecision,
    ) {
        self.audit.push(AuditLogEntry {
            plugin,
            capability: Capability::new("host", capability_id),
            operation: "host.call".into(),
            decision,
            timestamp_ms: 0,
        });
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
    /// v0.3: an optional theme-slot name the host resolves against its own token
    /// vocabulary (`chrome::S` slots). When absent — or naming a slot this host
    /// does not know — the span renders with its [`StyleRole`] instead, so an
    /// older host ignores it and an older plugin never sends it. Untrusted
    /// display data: the host resolves the name; it is never a host action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

impl Span {
    pub fn styled(text: impl Into<String>, role: StyleRole) -> Self {
        Self {
            text: text.into(),
            role,
            slot: None,
        }
    }

    /// A span that names a theme slot (v0.3); `role` is the fallback for a host
    /// that does not know the slot.
    pub fn slotted(text: impl Into<String>, role: StyleRole, slot: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            role,
            slot: Some(slot.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct View {
    /// The single-line content — the v0.2 compat path an older plugin keeps
    /// using. When [`View::rows`] is non-empty it is the effective content and
    /// this is the first row (or empty for a pure multi-row view).
    pub spans: Vec<Span>,
    /// v0.3: optional multi-row content (rows of spans). Empty means "single
    /// line" and the wire bytes are identical to v0.2 (skipped when empty), so
    /// an older host ignores it and an older plugin never sends it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Vec<Span>>,
}

impl View {
    pub fn line(spans: impl IntoIterator<Item = Span>) -> Self {
        Self {
            spans: spans.into_iter().collect(),
            rows: Vec::new(),
        }
    }

    /// A multi-row view (v0.3). The first row is mirrored into [`View::spans`]
    /// so a v0.2 host still shows a sensible single line.
    pub fn multi(rows: impl IntoIterator<Item = Vec<Span>>) -> Self {
        let rows: Vec<Vec<Span>> = rows.into_iter().collect();
        let spans = rows.first().cloned().unwrap_or_default();
        Self { spans, rows }
    }

    /// The effective rows to render: the multi-row `rows` when present, else the
    /// single-line `spans` as one row. The host's row budget/truncation applies
    /// on top of this.
    pub fn effective_rows(&self) -> Vec<Vec<Span>> {
        if self.rows.is_empty() {
            vec![self.spans.clone()]
        } else {
            self.rows.clone()
        }
    }

    pub fn text_content(&self) -> String {
        if self.rows.is_empty() {
            self.spans.iter().map(|s| s.text.as_str()).collect()
        } else {
            self.rows
                .iter()
                .map(|row| row.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, schemars::JsonSchema)]
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

/// The host→plugin request method for provider extension points: params are
/// `{"seam": "issues", "op": "<trait method>", "args": {…}}`; the plugin
/// answers the request's `id` with an [`RpcResponse`] whose `result` is the
/// op's return value, or an [`RpcError`] (`unsupported` maps to the seam's
/// optional-op fall-through). See `openspec/specs/plugin-runtime`.
pub const PROVIDER_CALL_METHOD: &str = "provider.call";

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

/// Stable support classification for extension-point and verb inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportState {
    Wired,
    SeparateAdapter,
    Reserved,
}

impl SupportState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wired => "wired",
            Self::SeparateAdapter => "separate_adapter",
            Self::Reserved => "reserved",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CadenceKind {
    OnDemand,
    Interval,
    OnEvent,
}

impl CadenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OnDemand => "on_demand",
            Self::Interval => "interval",
            Self::OnEvent => "on_event",
        }
    }
}

/// One authoritative fact row for extension-point negotiation in the general
/// compositor host. This is deliberately not a wire type: it describes what
/// the current host implements, while the wire enum remains forward-compatible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPointSupport {
    pub extension_point: ExtensionPoint,
    pub since: ApiVersion,
    pub state: SupportState,
    pub required_capability: Option<&'static str>,
    pub modes: &'static [PluginMode],
    pub cadences: &'static [CadenceKind],
    pub callbacks: &'static [PluginCallback],
    pub owner: &'static str,
}

impl ExtensionPointSupport {
    pub fn unavailable_reason(&self) -> String {
        match self.state {
            SupportState::Wired => format!(
                "unsupported extension point {}: not enabled by this host contract",
                self.extension_point.wire_name()
            ),
            SupportState::SeparateAdapter => format!(
                "unsupported extension point {} in the general plugin host: available only through {}",
                self.extension_point.wire_name(),
                self.owner
            ),
            SupportState::Reserved => format!(
                "unsupported extension point {}: reserved by {}",
                self.extension_point.wire_name(),
                self.owner
            ),
        }
    }

    pub fn mode_names(&self) -> Vec<&'static str> {
        self.modes.iter().map(|mode| mode.wire_name()).collect()
    }

    pub fn cadence_names(&self) -> Vec<&'static str> {
        self.cadences
            .iter()
            .map(|cadence| cadence.as_str())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostVerbSupport {
    pub verb: HostVerb,
    pub since: ApiVersion,
    pub state: SupportState,
    pub authority: &'static str,
    pub modes: &'static [PluginMode],
    pub owner: &'static str,
}

const BOTH_MODES: &[PluginMode] = &[PluginMode::OneShot, PluginMode::Resident];
const RESIDENT_MODE: &[PluginMode] = &[PluginMode::Resident];
const ON_DEMAND_INTERVAL: &[CadenceKind] = &[CadenceKind::OnDemand, CadenceKind::Interval];
const ON_DEMAND: &[CadenceKind] = &[CadenceKind::OnDemand];
const NO_CALLBACKS: &[PluginCallback] = &[];
const RESIDENT_LIFECYCLE: &[PluginCallback] = &[
    PluginCallback::Activate,
    PluginCallback::Render,
    PluginCallback::Deactivate,
];
const PALETTE_CALLBACKS: &[PluginCallback] = &[
    PluginCallback::Activate,
    PluginCallback::OnEvent,
    PluginCallback::Deactivate,
];
const PROVIDER_CALLBACKS: &[PluginCallback] =
    &[PluginCallback::Activate, PluginCallback::Deactivate];

/// Complete current-host extension vocabulary. Consumers MUST derive runtime
/// negotiation and public support output from this table instead of matching
/// the wire enum independently.
pub static HOST_EXTENSION_SUPPORT: &[ExtensionPointSupport] = &[
    ExtensionPointSupport {
        extension_point: ExtensionPoint::StatusBarSegment,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        required_capability: Some("surface:statusbar"),
        modes: BOTH_MODES,
        cadences: ON_DEMAND_INTERVAL,
        callbacks: RESIDENT_LIFECYCLE,
        owner: "general compositor statusbar runtime",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::PanelSection,
        since: ApiVersion::new(0, 3, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:panel"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "THE-108",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::SidebarTab,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:sidebar"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "THE-107",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::PaletteAction,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        required_capability: Some("surface:palette"),
        modes: BOTH_MODES,
        cadences: ON_DEMAND,
        callbacks: PALETTE_CALLBACKS,
        owner: "general compositor palette runtime",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::NotificationSource,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        required_capability: Some("surface:notification"),
        modes: BOTH_MODES,
        cadences: ON_DEMAND_INTERVAL,
        callbacks: RESIDENT_LIFECYCLE,
        owner: "general compositor notification runtime",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::HarnessAdapter,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:harness"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "add-agent-harness-seam",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::ProgramAdapter,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:program"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "no accepted runtime owner",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::Theme,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:theme"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "THE-107",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::Automation,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:automation"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "native automation engine; no plugin adapter",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::DataSource,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::SeparateAdapter,
        required_capability: Some("surface:data"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "calendar command-account adapter",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::IssueProvider,
        since: ApiVersion::new(0, 2, 0),
        state: SupportState::Wired,
        required_capability: Some("surface:provider"),
        modes: RESIDENT_MODE,
        cadences: ON_DEMAND,
        callbacks: PROVIDER_CALLBACKS,
        owner: "issue provider bridge",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::CiProvider,
        since: ApiVersion::new(0, 2, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:provider"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "no dynamic CI provider selector",
    },
    ExtensionPointSupport {
        extension_point: ExtensionPoint::ForgeProvider,
        since: ApiVersion::new(0, 2, 0),
        state: SupportState::Reserved,
        required_capability: Some("surface:provider"),
        modes: &[],
        cadences: &[],
        callbacks: NO_CALLBACKS,
        owner: "no dynamic forge provider selector",
    },
];

/// Complete plugin→host verb contract. `host.call` is resident-only because a
/// one-shot process has no reply channel; the remaining request/notification
/// verbs are handled by the runtime dispatcher in both modes.
pub static HOST_VERB_SUPPORT: &[HostVerbSupport] = &[
    HostVerbSupport {
        verb: HostVerb::Register,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        authority: "surface:<contribution>",
        modes: BOTH_MODES,
        owner: "plugin runtime",
    },
    HostVerbSupport {
        verb: HostVerb::Subscribe,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        authority: "none",
        modes: RESIDENT_MODE,
        owner: "event bridge",
    },
    HostVerbSupport {
        verb: HostVerb::Update,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        authority: "surface:<registered>",
        modes: BOTH_MODES,
        owner: "surface cache",
    },
    HostVerbSupport {
        verb: HostVerb::Invalidate,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        authority: "surface:<registered>",
        modes: BOTH_MODES,
        owner: "surface cache",
    },
    HostVerbSupport {
        verb: HostVerb::Io,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        authority: "declared scheme:target capability",
        modes: BOTH_MODES,
        owner: "plugin runtime",
    },
    HostVerbSupport {
        verb: HostVerb::Notify,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        authority: "notify:<source>",
        modes: BOTH_MODES,
        owner: "notification bridge",
    },
    HostVerbSupport {
        verb: HostVerb::Emit,
        since: ApiVersion::new(0, 1, 0),
        state: SupportState::Wired,
        authority: "none",
        modes: BOTH_MODES,
        owner: "event bridge",
    },
    HostVerbSupport {
        verb: HostVerb::StateGet,
        since: ApiVersion::new(0, 2, 0),
        state: SupportState::Wired,
        authority: "state:<plugin>",
        modes: RESIDENT_MODE,
        owner: "namespaced state",
    },
    HostVerbSupport {
        verb: HostVerb::StateSet,
        since: ApiVersion::new(0, 2, 0),
        state: SupportState::Wired,
        authority: "state:<plugin>",
        modes: BOTH_MODES,
        owner: "namespaced state",
    },
    HostVerbSupport {
        verb: HostVerb::HostValue,
        since: ApiVersion::new(0, 2, 0),
        state: SupportState::Wired,
        authority: "none",
        modes: RESIDENT_MODE,
        owner: "host value projection",
    },
    HostVerbSupport {
        verb: HostVerb::HostCall,
        since: ApiVersion::new(0, 2, 0),
        state: SupportState::Wired,
        authority: "catalog-required control scope",
        modes: RESIDENT_MODE,
        owner: "capability catalog dispatcher",
    },
];

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

/// Host capabilities reachable from plugins via `host.call`, by catalog id —
/// **derived from the catalog**: every row listing [`Surface::Plugin`] except
/// streaming rows (the event feed is bridged separately, as `on_event`
/// notifications). The host runtime dispatches these generically through the
/// same capability→route spine `thegn api call` uses (scope-checked first), so
/// a newly routed catalog verb becomes plugin-callable with no per-verb code.
///
/// [`Surface::Plugin`]: crate::capability::Surface::Plugin
pub fn plugin_host_call_caps() -> Vec<&'static str> {
    use crate::capability::{Surface, for_surface};
    for_surface(Surface::Plugin)
        .filter(|c| !c.verb.is_streaming())
        .map(|c| c.id.as_str())
        .collect()
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    #[test]
    fn v0_3_version_and_panel_section_capability() {
        // The bump is 0.3.0, and the new rendering surface maps to a "panel"
        // surface capability beside statusbar — covering the new match arm.
        assert_eq!(API_VERSION, ApiVersion::new(0, 3, 0));
        assert_eq!(
            surface_capability_for(&ExtensionPoint::PanelSection),
            Some(Capability::new("surface", "panel"))
        );
        // The existing surfaces are unchanged.
        assert_eq!(
            surface_capability_for(&ExtensionPoint::StatusBarSegment),
            Some(Capability::new("surface", "statusbar"))
        );
        // PanelSection round-trips through the wire enum.
        let j = serde_json::to_value(ExtensionPoint::PanelSection).unwrap();
        assert_eq!(j, serde_json::json!("PanelSection"));
        let back: ExtensionPoint = serde_json::from_value(j).unwrap();
        assert_eq!(back, ExtensionPoint::PanelSection);
    }

    #[test]
    fn capability_helpers_and_every_surface_mapping_are_stable() {
        assert_eq!(ApiVersion::new(1, 2, 3).to_string(), "1.2.3");
        assert!(Capability::parse("network").is_none());
        let parsed = Capability::parse("network:api.example.com:443").unwrap();
        assert_eq!(parsed.kind(), "network");
        assert_eq!(parsed.target(), "api.example.com:443");
        assert_eq!(parsed.as_str(), "network:api.example.com:443");
        assert_eq!(parsed.to_string(), "network:api.example.com:443");

        for (point, target) in [
            (ExtensionPoint::StatusBarSegment, Some("statusbar")),
            (ExtensionPoint::PanelSection, Some("panel")),
            (ExtensionPoint::SidebarTab, Some("sidebar")),
            (ExtensionPoint::PaletteAction, Some("palette")),
            (ExtensionPoint::NotificationSource, Some("notification")),
            (ExtensionPoint::IssueProvider, Some("provider")),
            (ExtensionPoint::CiProvider, Some("provider")),
            (ExtensionPoint::ForgeProvider, Some("provider")),
            (ExtensionPoint::HarnessAdapter, Some("harness")),
            (ExtensionPoint::ProgramAdapter, Some("program")),
            (ExtensionPoint::Theme, Some("theme")),
            (ExtensionPoint::Automation, Some("automation")),
            (ExtensionPoint::DataSource, Some("data")),
            (ExtensionPoint::Unknown("future".into()), None),
        ] {
            assert_eq!(
                surface_capability_for(&point)
                    .as_ref()
                    .map(Capability::target),
                target,
                "{}",
                point.wire_name()
            );
        }
    }

    #[test]
    fn plugin_api_errors_have_actionable_diagnostics() {
        let cases = [
            (
                PluginApiError::IncompatibleApi {
                    required: ApiVersion::new(0, 3, 0),
                    got: ApiVersion::new(1, 0, 0),
                },
                "incompatible api: host ApiVersion { major: 0, minor: 3, patch: 0 }, plugin ApiVersion { major: 1, minor: 0, patch: 0 }",
            ),
            (
                PluginApiError::CapabilityDenied {
                    capability: Capability::new("network", "api.example.com"),
                    operation: "io.network".into(),
                },
                "denied: capability \"network:api.example.com\" required for io.network",
            ),
            (
                PluginApiError::UnsupportedExtensionPoint("PanelSection".into()),
                "unsupported extension point: PanelSection",
            ),
            (
                PluginApiError::UnknownExtensionPoint("FutureSurface".into()),
                "unknown extension point: FutureSurface",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn v0_2_view_json_still_decodes_into_a_single_line() {
        // A v0.2 wire view — only `spans`, role styling, no `slot`/`rows` — must
        // decode unchanged (an older plugin never sends the new fields).
        let v: View =
            serde_json::from_str(r#"{"spans":[{"text":"main","role":"Accent"}]}"#).unwrap();
        assert!(v.rows.is_empty(), "no multi-row content");
        assert_eq!(v.spans.len(), 1);
        assert_eq!(v.spans[0].slot, None, "no slot on a v0.2 span");
        assert_eq!(v.text_content(), "main");
        assert_eq!(v.effective_rows(), vec![v.spans.clone()]);
    }

    #[test]
    fn single_line_view_serializes_identically_to_v0_2() {
        // `skip_serializing_if` keeps a role-only single-line view byte-identical
        // on the wire: no `rows`, no `slot` keys, so an older host sees exactly
        // what it saw before the bump.
        let v = View::line([Span::styled("x", StyleRole::Accent)]);
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(
            j,
            serde_json::json!({"spans":[{"text":"x","role":"Accent"}]})
        );
        assert!(j.get("rows").is_none());
        assert!(j["spans"][0].get("slot").is_none());
    }

    #[test]
    fn v0_3_multi_row_and_slot_round_trip() {
        let v = View::multi([
            vec![Span::slotted("head", StyleRole::Accent, "accent")],
            vec![Span::styled("body", StyleRole::Default)],
        ]);
        // The first row is mirrored into `spans` for a v0.2 host.
        assert_eq!(
            v.spans,
            vec![Span::slotted("head", StyleRole::Accent, "accent")]
        );
        assert_eq!(v.effective_rows().len(), 2);
        assert_eq!(v.text_content(), "head\nbody");
        // Full round-trip through JSON preserves rows + slot.
        let j = serde_json::to_string(&v).unwrap();
        let back: View = serde_json::from_str(&j).unwrap();
        assert_eq!(back, v);
        assert_eq!(back.rows[0][0].slot.as_deref(), Some("accent"));
    }

    #[test]
    fn unknown_slot_name_is_kept_not_rejected() {
        // A slot the host does not know is not a decode error — the host falls
        // back to the role at render time; the wire keeps the name and the role.
        let s: Span =
            serde_json::from_str(r#"{"text":"x","role":"Warning","slot":"nonexistent"}"#).unwrap();
        assert_eq!(s.slot.as_deref(), Some("nonexistent"));
        assert_eq!(s.role, StyleRole::Warning);
    }

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
            let _ = c.class(); // best-effort: test smoke: this path must not panic
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
    fn malformed_api_versions_fail_instead_of_negotiating_as_zero() {
        for api in ["not-a-version", "0.3", "0.3.0.1", "0.three.0"] {
            let source = format!(
                r#"
id = "bad-version"
name = "Bad version"
version = "1.0.0"
api = "{api}"
command = ["true"]
"#
            );
            let error = toml::from_str::<PluginSpec>(&source).unwrap_err();
            assert!(
                error.to_string().contains("plugin API version"),
                "{api}: {error}"
            );
        }
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
        let host = HostContract::new(API_VERSION)
            .with_extension_points([ExtensionPoint::StatusBarSegment]);
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
    fn callback_notifications_and_events_round_trip() {
        for (callback, method) in [
            (PluginCallback::Activate, "activate"),
            (PluginCallback::OnEvent, "on_event"),
            (PluginCallback::Render, "render"),
            (PluginCallback::Deactivate, "deactivate"),
        ] {
            let message = RpcMessage::notification(callback, serde_json::json!({"key": method}));
            assert_eq!(message.id, None);
            assert_eq!(message.method(), Some(method));
            let encoded = serde_json::to_string(&message).unwrap();
            assert_eq!(
                Frame::parse_line(&encoded).unwrap(),
                Frame::Message(message)
            );
        }

        let event = Event::new(
            EventKind::WorktreeChanged,
            serde_json::json!({"path": "/work", "branch": "feature"}),
        );
        assert_eq!(event.kind, EventKind::WorktreeChanged);
        assert_eq!(event.payload["branch"], "feature");
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
        use crate::capability::{Surface, coverage_problems, for_surface};
        // The plugin surface is fully covered: `host.call` reaches every
        // non-streaming plugin row (derived set), and the resident-plugin feed
        // subscribe bridge delivers the streaming rows (the event feed) as
        // `on_event` notifications. Together they are every plugin row.
        let mut implemented = plugin_host_call_caps();
        implemented.extend(
            for_surface(Surface::Plugin)
                .filter(|c| c.verb.is_streaming())
                .map(|c| c.id.as_str()),
        );
        let problems = coverage_problems(Surface::Plugin, &implemented);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn plugin_host_call_caps_excludes_streams_and_admin() {
        // The event feed streams — it is bridged, not host-called.
        let set = plugin_host_call_caps();
        assert!(
            !set.contains(&"events.subscribe"),
            "feed is not a host.call"
        );
        assert!(set.contains(&"sessions.list"));
        assert!(set.contains(&"git.commit"));
        assert!(set.contains(&"tools.run"));
        for local_only in ["launch.preset", "containers.list", "containers.control"] {
            assert!(
                !set.contains(&local_only),
                "{local_only} has no generic control route and must not be advertised to plugins"
            );
        }
        // No admin row reaches the plugin surface (pinned in the catalog too).
        for cap in &set {
            let c = crate::capability::lookup(cap).unwrap();
            assert_ne!(
                crate::control::required_scope(c.verb),
                crate::control::Scope::Admin,
                "{cap} is admin-scoped but plugin-callable"
            );
        }
    }

    fn runtime_contribution(
        id: &str,
        point: ExtensionPoint,
        surface: Option<&str>,
    ) -> Contribution {
        Contribution {
            id: ContributionId::new(id),
            extension_point: point,
            label: id.into(),
            surface: surface.map(SurfaceId::new),
            cadence: CadenceHint::OnDemand,
            metadata: Default::default(),
            caps: serde_json::Value::Null,
            chord: None,
        }
    }

    #[test]
    fn runtime_enforces_capabilities_and_records_all_plugin_interactions() {
        let plugin = PluginId::new("runtime-test");
        let primary_surface = SurfaceId::new("runtime-test.primary");
        let manifest = NegotiatedManifest {
            api: API_VERSION,
            granted: [
                Capability::new("surface", "statusbar"),
                Capability::new("network", "api.example.com"),
                Capability::new("run", "git"),
                Capability::new("notify", "build"),
                Capability::new("state", plugin.as_str()),
            ]
            .into_iter()
            .collect(),
            accepted_contributions: vec![runtime_contribution(
                "primary",
                ExtensionPoint::StatusBarSegment,
                Some("runtime-test.primary"),
            )],
            supported_extension_points: [ExtensionPoint::StatusBarSegment].into_iter().collect(),
            ..NegotiatedManifest::default()
        };
        let mut runtime = PluginRuntime::new(manifest)
            .with_host_value("theme", serde_json::json!({"name": "dark"}));

        assert_eq!(
            runtime.host_value(plugin.clone(), "theme").unwrap(),
            Some(serde_json::json!({"name": "dark"}))
        );
        assert_eq!(runtime.host_value(plugin.clone(), "missing").unwrap(), None);

        assert!(runtime.is_dirty(&primary_surface));
        let first = View::line([Span::styled("ready", StyleRole::Accent)]);
        assert!(
            runtime
                .update(plugin.clone(), primary_surface.clone(), first.clone())
                .unwrap()
                .changed
        );
        assert!(!runtime.is_dirty(&primary_surface));
        assert_eq!(runtime.view(&primary_surface), Some(&first));
        assert!(
            !runtime
                .update(plugin.clone(), primary_surface.clone(), first)
                .unwrap()
                .changed
        );
        runtime
            .invalidate(plugin.clone(), primary_surface.clone())
            .unwrap();
        assert!(runtime.is_dirty(&primary_surface));

        let secondary_surface = SurfaceId::new("runtime-test.secondary");
        runtime
            .register(
                plugin.clone(),
                runtime_contribution(
                    "secondary",
                    ExtensionPoint::StatusBarSegment,
                    Some("runtime-test.secondary"),
                ),
            )
            .unwrap();
        assert!(
            runtime
                .update(
                    plugin.clone(),
                    secondary_surface.clone(),
                    View::line([Span::styled("secondary", StyleRole::Default)]),
                )
                .unwrap()
                .changed
        );

        let unsupported = runtime
            .register(
                plugin.clone(),
                runtime_contribution("sidebar", ExtensionPoint::SidebarTab, Some("sidebar")),
            )
            .unwrap_err();
        assert_eq!(
            unsupported,
            PluginApiError::UnsupportedExtensionPoint("SidebarTab".into())
        );

        let unknown_surface = SurfaceId::new("runtime-test.unknown");
        let denied_update = match runtime.update(
            plugin.clone(),
            unknown_surface.clone(),
            View::line([Span::styled("denied", StyleRole::Error)]),
        ) {
            Err(error) => error,
            Ok(_) => panic!("an unregistered surface must be denied"),
        };
        assert_eq!(
            denied_update,
            PluginApiError::CapabilityDenied {
                capability: Capability::new("surface", "unknown"),
                operation: "update".into(),
            }
        );
        assert!(runtime.invalidate(plugin.clone(), unknown_surface).is_err());

        runtime
            .subscribe(plugin.clone(), EventKind::FocusChanged)
            .unwrap();
        runtime
            .subscribe(plugin.clone(), EventKind::FocusChanged)
            .unwrap();
        assert_eq!(runtime.subscriptions().len(), 1);

        let event = Event::new(EventKind::Action, serde_json::json!({"id": "build"}));
        runtime.emit(plugin.clone(), event.clone()).unwrap();
        assert_eq!(runtime.events(), &[event]);

        let network = IoRequest::network(
            "POST",
            "https://user@api.example.com:8443/v1/jobs?wait=true",
        );
        assert_eq!(network.payload, serde_json::json!({"method": "POST"}));
        assert_eq!(
            network.required_capability(),
            Capability::new("network", "api.example.com")
        );
        assert_eq!(
            runtime.io(plugin.clone(), network).unwrap(),
            IoResult {
                status: IoStatus::Accepted,
                body: None,
            }
        );

        let run = IoRequest::run("git", ["status", "--short"]);
        assert_eq!(
            run.payload,
            serde_json::json!({"args": ["status", "--short"]})
        );
        assert_eq!(run.required_capability(), Capability::new("run", "git"));
        assert_eq!(
            runtime.io(plugin.clone(), run).unwrap().status,
            IoStatus::Accepted
        );

        let denied_io = runtime
            .io(
                plugin.clone(),
                IoRequest {
                    scheme: "file".into(),
                    target: "/etc/shadow".into(),
                    payload: serde_json::Value::Null,
                },
            )
            .unwrap_err();
        assert!(matches!(
            denied_io,
            PluginApiError::CapabilityDenied { ref capability, ref operation }
                if capability == &Capability::new("file", "/etc/shadow") && operation == "io.file"
        ));

        let alert = Alert::new("build", "completed");
        assert_eq!(alert.message, "completed");
        runtime.notify(plugin.clone(), alert).unwrap();
        assert!(
            runtime
                .notify(plugin.clone(), Alert::new("security", "denied"))
                .is_err()
        );

        runtime
            .state_set(plugin.clone(), "count", serde_json::json!(2))
            .unwrap();
        assert_eq!(
            runtime.state_get(plugin.clone(), "count").unwrap(),
            Some(serde_json::json!(2))
        );
        assert_eq!(runtime.state_get(plugin.clone(), "missing").unwrap(), None);
        assert!(
            runtime
                .state_get(PluginId::new("other-plugin"), "count")
                .is_err()
        );

        runtime.record_host_call_decision(plugin.clone(), "sessions.list", AuditDecision::Granted);
        runtime.record_host_call_decision(plugin.clone(), "sessions.kill", AuditDecision::Denied);

        let audit = runtime.audit_log();
        assert!(audit.iter().any(|entry| {
            entry.plugin == plugin
                && entry.capability == Capability::new("network", "api.example.com")
                && entry.operation == "io.network"
                && entry.decision == AuditDecision::Granted
                && entry.timestamp_ms == 0
        }));
        assert!(audit.iter().any(|entry| {
            entry.capability == Capability::new("host", "sessions.kill")
                && entry.operation == "host.call"
                && entry.decision == AuditDecision::Denied
        }));
    }

    #[test]
    fn io_capability_parsing_strips_credentials_ports_and_paths() {
        for (url, host) in [
            ("https://api.example.com/v1", "api.example.com"),
            ("https://user@api.example.com:8443/v1", "api.example.com"),
            ("api.example.com?query=1", "api.example.com"),
            ("api.example.com#fragment", "api.example.com"),
        ] {
            assert_eq!(host_from_url(url), host);
            assert_eq!(
                IoRequest::network("GET", url).required_capability(),
                Capability::new("network", host)
            );
        }
    }

    #[test]
    fn surface_cache_tracks_changes_invalidation_and_degradation() {
        let mut cache = SurfaceCache::default();
        let missing = SurfaceId::new("missing");
        assert!(cache.is_dirty(&missing));
        assert!(cache.view(&missing).is_none());
        let degraded = cache.degrade(&missing, DegradeReason::Crash);
        assert!(degraded.degraded);
        assert_eq!(degraded.text_content(), "⚠");

        let surface = SurfaceId::new("status");
        let original = View::line([Span::styled("working", StyleRole::Default)]);
        assert!(cache.update(surface.clone(), original.clone()).changed);
        assert!(!cache.update(surface.clone(), original.clone()).changed);
        assert_eq!(cache.view(&surface), Some(&original));
        assert!(!cache.is_dirty(&surface));
        cache.invalidate(&surface);
        assert!(cache.is_dirty(&surface));

        let degraded = cache.degrade(&surface, DegradeReason::RenderBudgetExceeded);
        assert!(degraded.degraded);
        assert_eq!(degraded.text_content(), "working ⚠");
        assert_eq!(cache.view(&surface), Some(&original));
    }

    fn current_host_contract() -> HostContract {
        HostContract::new(API_VERSION)
            .with_extension_support(HOST_EXTENSION_SUPPORT.iter().cloned())
            .with_grants(
                HOST_EXTENSION_SUPPORT
                    .iter()
                    .filter_map(|row| row.required_capability)
                    .filter_map(Capability::parse),
            )
    }

    fn support_spec(
        point: ExtensionPoint,
        capabilities: Vec<Capability>,
        mode: PluginMode,
    ) -> PluginSpec {
        PluginSpec {
            manifest: PluginManifest {
                id: PluginId::new("support-test"),
                name: "Support test".into(),
                version: "1.0.0".into(),
                api: API_VERSION,
                capabilities,
                contributions: vec![Contribution {
                    id: ContributionId::new("support-test.row"),
                    extension_point: point,
                    label: "Support test".into(),
                    surface: Some(SurfaceId::new("support-test.surface")),
                    cadence: CadenceHint::OnDemand,
                    metadata: Default::default(),
                    caps: serde_json::Value::Null,
                    chord: None,
                }],
            },
            command: vec!["true".into()],
            cwd: String::new(),
            env: Default::default(),
            timeout_secs: 5,
            scopes: Vec::new(),
            mode,
            enabled: true,
        }
    }

    #[test]
    fn current_host_support_tables_are_complete_and_unique() {
        let expected_points = [
            "StatusBarSegment",
            "PanelSection",
            "SidebarTab",
            "PaletteAction",
            "NotificationSource",
            "HarnessAdapter",
            "ProgramAdapter",
            "Theme",
            "Automation",
            "DataSource",
            "IssueProvider",
            "CiProvider",
            "ForgeProvider",
        ];
        let actual = HOST_EXTENSION_SUPPORT
            .iter()
            .map(|row| row.extension_point.wire_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual.len(), HOST_EXTENSION_SUPPORT.len());
        assert_eq!(actual, expected_points.into_iter().collect());

        let verbs = HOST_VERB_SUPPORT
            .iter()
            .map(|row| row.verb)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(verbs.len(), HOST_VERB_SUPPORT.len());
        assert_eq!(verbs, HostVerb::ALL.iter().copied().collect());
    }

    #[test]
    fn support_negotiation_reports_missing_reserved_and_mode_reasons() {
        let host = current_host_contract();
        let missing = support_spec(
            ExtensionPoint::StatusBarSegment,
            Vec::new(),
            PluginMode::OneShot,
        );
        let neg = host.negotiate_spec(&missing).unwrap();
        assert!(neg.accepted_contributions.is_empty());
        assert_eq!(
            neg.rejected_contributions[0].reason,
            "extension point StatusBarSegment requires declared capability surface:statusbar"
        );

        let reserved = support_spec(
            ExtensionPoint::PanelSection,
            vec![Capability::new("surface", "panel")],
            PluginMode::Resident,
        );
        let neg = host.negotiate_spec(&reserved).unwrap();
        assert_eq!(
            neg.rejected_contributions[0].reason,
            "unsupported extension point PanelSection: reserved by THE-108"
        );

        let one_shot_provider = support_spec(
            ExtensionPoint::IssueProvider,
            vec![Capability::new("surface", "provider")],
            PluginMode::OneShot,
        );
        let neg = host.negotiate_spec(&one_shot_provider).unwrap();
        assert!(
            !neg.rejected_contributions[0]
                .reason
                .contains("requires declared capability")
        );
        assert!(
            neg.rejected_contributions[0]
                .reason
                .contains("does not support mode one_shot")
        );
    }

    #[test]
    fn negotiation_validates_versions_grants_cadence_and_support_metadata() {
        let host = current_host_contract();
        assert_eq!(host.extension_support().len(), HOST_EXTENSION_SUPPORT.len());

        for incompatible in [ApiVersion::new(1, 0, 0), ApiVersion::new(0, 4, 0)] {
            let mut spec = support_spec(
                ExtensionPoint::StatusBarSegment,
                vec![Capability::new("surface", "statusbar")],
                PluginMode::OneShot,
            );
            spec.manifest.api = incompatible;
            let error = match host.negotiate(&spec.manifest) {
                Err(error) => error,
                Ok(_) => panic!("incompatible plugin API must be rejected"),
            };
            assert_eq!(
                error,
                PluginApiError::IncompatibleApi {
                    required: API_VERSION,
                    got: incompatible,
                }
            );
        }

        let granted = Capability::new("surface", "statusbar");
        let denied = Capability::new("network", "private.example.com");
        let spec = support_spec(
            ExtensionPoint::StatusBarSegment,
            vec![granted.clone(), denied.clone()],
            PluginMode::OneShot,
        );
        let negotiated = host.negotiate(&spec.manifest).unwrap();
        assert!(negotiated.is_capability_granted(&granted));
        assert!(negotiated.is_capability_denied(&denied));
        assert!(!negotiated.is_capability_denied(&granted));

        let mut wrong_cadence = support_spec(
            ExtensionPoint::StatusBarSegment,
            vec![Capability::new("surface", "statusbar")],
            PluginMode::Resident,
        );
        wrong_cadence.manifest.contributions[0].cadence = CadenceHint::OnEvent {
            events: vec!["focus_changed".into()],
        };
        let negotiated = host.negotiate_spec(&wrong_cadence).unwrap();
        assert!(negotiated.accepted_contributions.is_empty());
        assert_eq!(
            negotiated.rejected_contributions[0].reason,
            "extension point StatusBarSegment does not support cadence on_event; supported cadences: on_demand,interval"
        );
        assert_eq!(
            CadenceHint::Interval { millis: 1 }.kind(),
            CadenceKind::Interval
        );
        assert_eq!(
            CadenceHint::OnEvent { events: vec![] }.kind(),
            CadenceKind::OnEvent
        );

        for (state, name) in [
            (SupportState::Wired, "wired"),
            (SupportState::SeparateAdapter, "separate_adapter"),
            (SupportState::Reserved, "reserved"),
        ] {
            assert_eq!(state.as_str(), name);
        }
        let statusbar = host.support_for(&ExtensionPoint::StatusBarSegment).unwrap();
        assert_eq!(statusbar.mode_names(), vec!["one_shot", "resident"]);
        assert_eq!(statusbar.cadence_names(), vec!["on_demand", "interval"]);
        assert_eq!(
            statusbar.unavailable_reason(),
            "unsupported extension point StatusBarSegment: not enabled by this host contract"
        );
        let data = host.support_for(&ExtensionPoint::DataSource).unwrap();
        assert_eq!(
            data.unavailable_reason(),
            "unsupported extension point DataSource in the general plugin host: available only through calendar command-account adapter"
        );
    }

    #[test]
    fn runtime_register_cannot_bypass_negotiated_support() {
        let host = current_host_contract();
        let spec = support_spec(
            ExtensionPoint::IssueProvider,
            vec![Capability::new("surface", "provider")],
            PluginMode::Resident,
        );
        let negotiated = host.negotiate_spec(&spec).unwrap();
        let mut runtime = PluginRuntime::new(negotiated);
        let mut forged = spec.manifest.contributions[0].clone();
        forged.extension_point = ExtensionPoint::CiProvider;
        let error = runtime
            .register(PluginId::new("support-test"), forged)
            .unwrap_err();
        assert!(matches!(
            error,
            PluginApiError::UnsupportedExtensionPoint(ref point) if point == "CiProvider"
        ));

        let one_shot_provider = support_spec(
            ExtensionPoint::IssueProvider,
            vec![Capability::new("surface", "provider")],
            PluginMode::OneShot,
        );
        let negotiated = host.negotiate_spec(&one_shot_provider).unwrap();
        let contribution = one_shot_provider.manifest.contributions[0].clone();
        let mut runtime = PluginRuntime::new(negotiated);
        let error = runtime
            .register(PluginId::new("support-test"), contribution)
            .unwrap_err();
        assert!(matches!(
            error,
            PluginApiError::UnsupportedExtensionPoint(ref point) if point == "IssueProvider"
        ));
    }
}
