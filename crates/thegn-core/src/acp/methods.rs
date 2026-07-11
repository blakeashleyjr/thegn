use serde::{Deserialize, Serialize};

use super::capabilities::{AgentCapabilities, AgentInfo, ClientCapabilities, ClientInfo};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: String,
    pub client_capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: String,
    pub agent_capabilities: AgentCapabilities,
    pub agent_info: AgentInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _meta: Option<PromptMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMeta {
    pub traceparent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvidersSetParams {
    pub id: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionUpdateEvent {
    AgentMessageChunk {
        content: String,
    },
    AgentThoughtChunk {
        content: String,
    },
    UsageUpdate {
        used: i64,
        size: i64,
    },
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolCallUpdate {
        tool_call_id: String,
        status: String,
        result: Option<serde_json::Value>,
    },
    ConfigOptionUpdate {
        option_id: String,
        value: serde_json::Value,
    },
    /// The agent finished all work for the current request (pi `agent_end`).
    /// Drives the `AgentDone`/`AgentFailed` notification + clears the chip's
    /// running state. Fires for user-driven turns too (not just thegn prompts).
    AgentEnd {
        #[serde(default = "default_true")]
        success: bool,
    },
}

fn default_true() -> bool {
    true
}
