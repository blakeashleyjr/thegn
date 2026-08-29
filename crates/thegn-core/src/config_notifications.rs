//! Pure configuration for notification and sound routing.
//!
//! This module is intentionally substrate-free. Sound paths and pack names are
//! data which the host resolves later; this layer only parses and validates the
//! user-facing vocabulary.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::{config_enum, config_warn};
use crate::config_defaults::default_true;
use crate::notification::{NotificationKind, Priority};
use crate::notification_sound::SoundRef;

fn default_agent_error_signatures() -> Vec<String> {
    crate::agent_error::AgentErrorSignatures::defaults().signatures
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationsConfig {
    pub desktop: bool,
    pub desktop_min_urgency: String,
    pub process_exit: String,
    #[serde(skip_serializing_if = "is_false")]
    pub surface_self_log_errors: bool,
    pub github_mentions: bool,
    pub agent_attention_inbox: bool,
    #[serde(default = "default_agent_error_signatures")]
    pub agent_error_signatures: Vec<String>,
    pub priority: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<NotificationRule>,
    pub dnd: DndConfig,
    pub sound: SoundConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub modes: BTreeMap<String, NotificationMode>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub active_mode: String,
    pub push: crate::config_push::PushConfig,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            desktop: true,
            desktop_min_urgency: "normal".into(),
            process_exit: "failures_and_tasks".into(),
            surface_self_log_errors: false,
            github_mentions: true,
            agent_attention_inbox: false,
            agent_error_signatures: default_agent_error_signatures(),
            priority: BTreeMap::new(),
            rules: Vec::new(),
            dnd: DndConfig::default(),
            sound: SoundConfig::default(),
            modes: BTreeMap::new(),
            active_mode: String::new(),
            push: crate::config_push::PushConfig::default(),
        }
    }
}

impl NotificationsConfig {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.agent_error_signatures.len() > crate::agent_error::MAX_AGENT_ERROR_SIGNATURES {
            errors.push(format!(
                "notifications.agent_error_signatures: more than {} entries",
                crate::agent_error::MAX_AGENT_ERROR_SIGNATURES
            ));
        }
        for (index, signature) in self.agent_error_signatures.iter().enumerate() {
            let key = format!("notifications.agent_error_signatures[{index}]");
            if signature.trim().is_empty() {
                errors.push(format!("{key}: empty (a signature must name something)"));
            }
            if signature.chars().count() > 256 {
                errors.push(format!("{key}: over 256 characters"));
            }
        }
        errors
    }

    pub fn validate_sound(&self) -> Vec<String> {
        self.sound.validate()
    }

    pub fn priority_of(&self, kind: NotificationKind) -> Priority {
        self.priority
            .get(kind.as_str())
            .and_then(|s| Priority::parse(s))
            .unwrap_or_else(|| kind.default_priority())
    }

    pub fn kind_names_at_or_above(&self, min: Priority) -> Vec<&'static str> {
        NotificationKind::ALL
            .into_iter()
            .filter(|kind| self.priority_of(*kind).rank() >= min.rank())
            .map(NotificationKind::as_str)
            .collect()
    }

    pub fn alert_kind_names(&self) -> Vec<&'static str> {
        self.kind_names_at_or_above(Priority::Alert)
    }

    pub fn counted_unread_kind_names(&self) -> Vec<&'static str> {
        self.kind_names_at_or_above(Priority::Notice)
    }

    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }
}

config_enum! {
    /// The notification sound mode. bell is the zero-config default.
    pub enum SoundMode: "notification sound mode" {
        Off = "off" | "none" | "silent",
        Chime = "chime" | "sound",
        Bell = "bell" | "beep" | "terminal",
        Command = "command" | "cmd" | "exec",
    } default = Bell;
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationRule {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_priority: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<Vec<String>>,
    #[serde(skip_serializing_if = "is_false")]
    pub mute: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub drop: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub stop: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DndConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<String>,
    pub allow_priority: String,
}

impl Default for DndConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            windows: Vec::new(),
            allow_priority: "alert".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SoundConfig {
    pub mute: bool,
    pub mode: SoundMode,
    pub min_priority: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub always_kinds: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub suppress_focused: bool,
    pub pack: String,
    pub volume: f32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub chime_file: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub per_priority: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub per_kind: BTreeMap<String, String>,
}

impl Default for SoundConfig {
    fn default() -> Self {
        Self {
            mute: false,
            mode: SoundMode::default(),
            min_priority: "alert".into(),
            always_kinds: vec![
                "agent_done".into(),
                "agent_attention".into(),
                "agent_failed".into(),
            ],
            suppress_focused: true,
            pack: String::new(),
            volume: 1.0,
            chime_file: String::new(),
            command: String::new(),
            per_priority: BTreeMap::new(),
            per_kind: BTreeMap::new(),
        }
    }
}

impl SoundConfig {
    pub fn clamped_volume(&self) -> f32 {
        if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.volume.is_finite() || !(0.0..=1.0).contains(&self.volume) {
            errors.push(format!(
                "notifications.sound.volume: expected a finite number from 0.0 to 1.0, got {}",
                self.volume
            ));
        }
        if !self.pack.trim().is_empty() && !SoundRef::is_user_path(self.pack.trim()) {
            errors.push(format!(
                "notifications.sound.pack: expected an absolute or ~-expanded directory path, got {:?}",
                self.pack
            ));
        }
        for kind in &self.always_kinds {
            if let Some(error) = validate_kind_name("notifications.sound.always_kinds", kind) {
                errors.push(error);
            }
        }
        for (kind, reference) in &self.per_kind {
            if let Some(error) = validate_kind_name("notifications.sound.per_kind", kind) {
                errors.push(error);
            }
            if let Err(error) = SoundRef::parse(reference) {
                errors.push(format!("notifications.sound.per_kind.{kind}: {error}"));
            }
        }
        if !self.chime_file.trim().is_empty()
            && let Err(error) = SoundRef::parse(&self.chime_file)
        {
            errors.push(format!("notifications.sound.chime_file: {error}"));
        }
        errors
    }
}

fn validate_kind_name(field: &str, raw: &str) -> Option<String> {
    if NotificationKind::ALL
        .iter()
        .any(|kind| kind.as_str() == raw.trim())
    {
        return None;
    }
    let suggestion = nearest_kind(raw.trim());
    Some(match suggestion {
        Some(kind) => format!("{field}: unknown kind {:?} (did you mean {kind}?)", raw),
        None => format!("{field}: unknown kind {:?}", raw),
    })
}

fn nearest_kind(input: &str) -> Option<&'static str> {
    let mut best = None;
    let mut best_distance = usize::MAX;
    for kind in NotificationKind::ALL {
        let distance = edit_distance(input, kind.as_str());
        if distance < best_distance {
            best_distance = distance;
            best = Some(kind.as_str());
        }
    }
    (best_distance <= 3).then_some(best).flatten()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let mut row: Vec<usize> = (0..=b.chars().count()).collect();
    for (i, ac) in a.chars().enumerate() {
        let mut next = vec![i + 1];
        for (j, bc) in b.chars().enumerate() {
            next.push(if ac == bc {
                row[j]
            } else {
                1 + row[j].min(row[j + 1]).min(next[j])
            });
        }
        row = next;
    }
    row[b.chars().count()]
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationMode {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationsOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_min_urgency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_exit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_self_log_errors: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<NotificationRule>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dnd: Option<DndConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound: Option<SoundConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<BTreeMap<String, NotificationMode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_mode: Option<String>,
}

impl NotificationsOverlay {
    pub fn is_empty(&self) -> bool {
        self.desktop.is_none()
            && self.desktop_min_urgency.is_none()
            && self.process_exit.is_none()
            && self.surface_self_log_errors.is_none()
            && self.priority.is_none()
            && self.rules.is_none()
            && self.dnd.is_none()
            && self.sound.is_none()
            && self.modes.is_none()
            && self.active_mode.is_none()
    }

    pub fn apply(self, base: &mut NotificationsConfig) {
        if let Some(value) = self.desktop {
            base.desktop = value;
        }
        if let Some(value) = self.desktop_min_urgency {
            base.desktop_min_urgency = value;
        }
        if let Some(value) = self.process_exit {
            base.process_exit = value;
        }
        if let Some(value) = self.surface_self_log_errors {
            base.surface_self_log_errors = value;
        }
        if let Some(value) = self.priority {
            base.priority = value;
        }
        if let Some(value) = self.rules {
            base.rules = value;
        }
        if let Some(value) = self.dnd {
            base.dnd = value;
        }
        if let Some(value) = self.sound {
            base.sound = value;
        }
        if let Some(value) = self.modes {
            base.modes = value;
        }
        if let Some(value) = self.active_mode {
            base.active_mode = value;
        }
    }
}
