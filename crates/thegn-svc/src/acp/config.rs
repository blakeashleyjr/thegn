use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpProxyConfig {
    pub port: u16,
    pub host: String,
}
