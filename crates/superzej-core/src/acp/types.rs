use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An ID that can be either a string or an integer, as per JSON-RPC 2.0.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    String(String),
    Number(i64),
}

/// A generic JSON-RPC 2.0 Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A generic JSON-RPC 2.0 Notification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A generic JSON-RPC 2.0 Response Result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseResult {
    pub jsonrpc: String,
    pub id: Id,
    pub result: Value,
}

/// A generic JSON-RPC 2.0 Response Error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub jsonrpc: String,
    pub id: Id,
    pub error: JsonRpcError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The top-level deserialization envelope
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(Request),
    ResponseResult(ResponseResult),
    ResponseError(ResponseError),
    Notification(Notification),
}

impl Request {
    pub fn new(id: Id, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}
