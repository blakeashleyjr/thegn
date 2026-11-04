//! Typed app input and local-acceptance results. Display revisions are not mutation locks.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InteractionBinding {
    #[serde(default)]
    pub target: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precondition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AppEventParams {
    pub surface: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub state_scope: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_ticket: Option<String>,
    #[serde(flatten)]
    pub event: AppEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum AppEvent {
    Action(Interaction),
    Activate(Interaction),
    Submit(Submission),
    Cancel(FormIdentity),
    Change(FormEdit),
    Select(Selection),
    Key(KeyInput),
    Paste {
        widget: String,
        text: String,
    },
    Resize {
        widget: String,
        width: u16,
        height: u16,
    },
    Focus {
        focused: bool,
    },
    Visibility {
        visible: bool,
        #[serde(default)]
        background: bool,
    },
    Invoke {
        id: String,
    },
    Sort {
        widget: String,
        column: String,
        descending: bool,
    },
    Closed,
}

impl AppEvent {
    pub fn reliable(&self) -> bool {
        matches!(
            self,
            Self::Action(_)
                | Self::Activate(_)
                | Self::Submit(_)
                | Self::Cancel(_)
                | Self::Invoke { .. }
                | Self::Sort { .. }
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Interaction {
    pub widget: String,
    /// Stable action/row/node identity, never a display label.
    pub id: String,
    #[serde(default)]
    pub binding: InteractionBinding,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FormIdentity {
    pub widget: String,
    pub instance: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Submission {
    pub widget: String,
    pub instance: String,
    pub values: BTreeMap<String, String>,
    pub draft_version: u64,
    #[serde(default)]
    pub binding: InteractionBinding,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FormEdit {
    pub widget: String,
    pub instance: String,
    pub values: BTreeMap<String, String>,
    pub draft_version: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Selection {
    pub widget: String,
    pub items: Vec<String>,
    pub current: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KeyInput {
    pub widget: String,
    pub key: NamedKey,
    #[serde(default)]
    pub modifiers: KeyModifiers,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "name", content = "value", rename_all = "snake_case")]
pub enum NamedKey {
    Character(String),
    Enter,
    Escape,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct KeyModifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

/// Empty successful results acknowledge completed local acceptance. Field
/// errors preserve the caller's submitted snapshot and do not acknowledge a save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AppEventResult {
    #[serde(default)]
    pub field_errors: BTreeMap<String, String>,
    #[serde(default)]
    pub message: String,
}

impl AppEventParams {
    /// Reject expensive or ambiguous input before dispatching application logic.
    pub fn validate(&self) -> Result<(), String> {
        if !super::valid_id(&self.surface)
            || self.session.len() > 256
            || self.session.chars().any(char::is_control)
            || self.state_scope.len() > 256
            || self.state_scope.chars().any(char::is_control)
            || self
                .focus_ticket
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err("invalid event envelope".into());
        }
        let valid_values = |values: &BTreeMap<String, String>| {
            values.len() <= 64
                && values
                    .iter()
                    .all(|(k, v)| super::valid_id(k) && v.len() <= 32 * 1024 && super::safe_text(v))
        };
        let valid_instance = |widget: &str, instance: &str| {
            super::valid_id(widget)
                && instance.len() <= 256
                && !instance.chars().any(char::is_control)
        };
        let ok = match &self.event {
            AppEvent::Action(i) | AppEvent::Activate(i) => {
                super::valid_id(&i.widget)
                    && super::valid_id(&i.id)
                    && super::valid_binding(&i.binding)
            }
            AppEvent::Submit(s) => {
                valid_instance(&s.widget, &s.instance)
                    && valid_values(&s.values)
                    && super::valid_binding(&s.binding)
            }
            AppEvent::Change(s) => {
                valid_instance(&s.widget, &s.instance) && valid_values(&s.values)
            }
            AppEvent::Cancel(s) => valid_instance(&s.widget, &s.instance),
            AppEvent::Select(s) => {
                let mut ids = std::collections::HashSet::new();
                super::valid_id(&s.widget)
                    && s.items.len() <= 4096
                    && s.items
                        .iter()
                        .all(|id| super::valid_id(id) && ids.insert(id))
                    && s.current.as_ref().is_none_or(|id| super::valid_id(id))
            }
            AppEvent::Key(k) => {
                super::valid_id(&k.widget)
                    && match &k.key {
                        NamedKey::Character(c) => {
                            c.len() <= 64 && !c.is_empty() && !c.chars().any(char::is_control)
                        }
                        NamedKey::Function(n) => (1..=35).contains(n),
                        _ => true,
                    }
            }
            AppEvent::Paste { widget, text } => {
                super::valid_id(widget) && text.len() <= 32 * 1024 && super::safe_text(text)
            }
            AppEvent::Resize { widget, .. } => super::valid_id(widget),
            AppEvent::Invoke { id } => super::valid_id(id),
            AppEvent::Sort { widget, column, .. } => {
                super::valid_id(widget) && super::valid_id(column)
            }
            _ => true,
        };
        if !ok {
            return Err("invalid app event data".into());
        }
        // Input already crosses a bounded transport. This second aggregate
        // check also protects SDK users dispatching constructed events directly.
        if serde_json::to_vec(self)
            .map_err(|_| "cannot encode app event")?
            .len()
            >= 1024 * 1024
        {
            return Err("app event exceeds encoded budget".into());
        }
        Ok(())
    }
}
