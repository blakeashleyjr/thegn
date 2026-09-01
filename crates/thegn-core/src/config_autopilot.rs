//! Configuration for the opt-in issue-to-PR supervisor.

use crate::config::{config_enum, config_warn};
use serde::{Deserialize, Serialize};

config_enum! {
    /// How an autopilot-created pull request is opened.
    pub enum AutopilotOpenAs : "autopilot PR mode" {
        Ready = "ready",
        Draft = "draft",
    } default = Ready;
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AutopilotConfig {
    pub enabled: bool,
    pub trigger_label: String,
    pub assignee: String,
    #[schemars(with = "String")]
    pub pickup_status: crate::issue::IssueStatus,
    pub max_concurrent: u32,
    pub max_attempts: u32,
    pub agent: String,
    pub agent_command: String,
    pub agent_timeout_secs: u64,
    pub open_as: AutopilotOpenAs,
    pub done_on_merge: bool,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_label: "agent-ready".into(),
            assignee: "me".into(),
            pickup_status: crate::issue::IssueStatus::Todo,
            max_concurrent: 1,
            max_attempts: 1,
            agent: String::new(),
            agent_command: String::new(),
            agent_timeout_secs: 1800,
            open_as: AutopilotOpenAs::Ready,
            done_on_merge: false,
        }
    }
}

impl AutopilotConfig {
    pub fn validate(&self, prefix: &str) -> Vec<String> {
        let mut errors = Vec::new();
        let key = |name: &str| format!("{prefix}.{name}");
        if self.assignee.trim() != "me" {
            errors.push(format!("{}: only \"me\" is supported", key("assignee")));
        }
        if self.trigger_label.is_empty() {
            errors.push(format!("{}: must not be empty", key("trigger_label")));
        }
        if self.max_concurrent == 0 {
            errors.push(format!("{}: must be at least 1", key("max_concurrent")));
        }
        if self.max_attempts == 0 {
            errors.push(format!("{}: must be at least 1", key("max_attempts")));
        }
        if self.agent_command.contains('\n') || self.agent_command.contains('\r') {
            errors.push(format!("{}: must be a single line", key("agent_command")));
        }
        errors
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AutopilotOverlay {
    pub enabled: Option<bool>,
    pub trigger_label: Option<String>,
    pub assignee: Option<String>,
    #[schemars(with = "Option<String>")]
    pub pickup_status: Option<crate::issue::IssueStatus>,
    pub max_concurrent: Option<u32>,
    pub max_attempts: Option<u32>,
    pub agent: Option<String>,
    pub agent_command: Option<String>,
    pub agent_timeout_secs: Option<u64>,
    pub open_as: Option<AutopilotOpenAs>,
    pub done_on_merge: Option<bool>,
}

impl AutopilotOverlay {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.trigger_label.is_none()
            && self.assignee.is_none()
            && self.pickup_status.is_none()
            && self.max_concurrent.is_none()
            && self.max_attempts.is_none()
            && self.agent.is_none()
            && self.agent_command.is_none()
            && self.agent_timeout_secs.is_none()
            && self.open_as.is_none()
            && self.done_on_merge.is_none()
    }

    pub fn apply(self, base: &mut AutopilotConfig) {
        if let Some(v) = self.enabled {
            base.enabled = v;
        }
        if let Some(v) = self.trigger_label {
            base.trigger_label = v;
        }
        if let Some(v) = self.assignee {
            base.assignee = v;
        }
        if let Some(v) = self.pickup_status {
            base.pickup_status = v;
        }
        if let Some(v) = self.max_concurrent {
            base.max_concurrent = v;
        }
        if let Some(v) = self.max_attempts {
            base.max_attempts = v;
        }
        if let Some(v) = self.agent {
            base.agent = v;
        }
        if let Some(v) = self.agent_command {
            base.agent_command = v;
        }
        if let Some(v) = self.agent_timeout_secs {
            base.agent_timeout_secs = v;
        }
        if let Some(v) = self.open_as {
            base.open_as = v;
        }
        if let Some(v) = self.done_on_merge {
            base.done_on_merge = v;
        }
    }

    pub fn validate(&self, prefix: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some(value) = &self.assignee
            && value.trim() != "me"
        {
            errors.push(format!("{prefix}.assignee: only \"me\" is supported"));
        }
        if self.trigger_label.as_deref() == Some("") {
            errors.push(format!("{prefix}.trigger_label: must not be empty"));
        }
        if self.max_concurrent == Some(0) {
            errors.push(format!("{prefix}.max_concurrent: must be at least 1"));
        }
        if self.max_attempts == Some(0) {
            errors.push(format!("{prefix}.max_attempts: must be at least 1"));
        }
        if self
            .agent_command
            .as_deref()
            .is_some_and(|value| value.contains(['\n', '\r']))
        {
            errors.push(format!("{prefix}.agent_command: must be a single line"));
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_off_and_safe() {
        let c = AutopilotConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.assignee, "me");
        assert_eq!(c.max_concurrent, 1);
        assert_eq!(c.max_attempts, 1);
    }
    #[test]
    fn unknown_assignee_is_rejected() {
        let c = AutopilotConfig {
            assignee: "anyone".into(),
            ..Default::default()
        };
        assert!(!c.validate("autopilot").is_empty());
    }
}
