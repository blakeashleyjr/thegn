//! Fly.io **GraphQL** request/response shaping — **pure** functions only, for the
//! one capability the Machines REST API does not cover: allocating a public
//! **dedicated IPv4** to an app so thegn can reach the machine's sshd over the
//! Fly proxy (the CLI-free equivalent of `fly ips allocate-v4`).
//!
//! ⚠ **This is Fly's GraphQL API** (`https://api.fly.io/graphql`); Fly documents
//! it as less stable than the Machines REST API. thegn only reaches for it
//! because IP allocation is absent from Machines REST. The shaping here is
//! unit-tested; the live calls are exercised by `fly_live`.

use anyhow::{Result, anyhow};

pub const DEFAULT_GRAPHQL_URL: &str = "https://api.fly.io/graphql";

/// A GraphQL request body (`{ query, variables }`).
pub fn request(query: &str, variables: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "query": query, "variables": variables })
}

/// Extract `data`, surfacing `errors[].message` as an `Err` (GraphQL returns 200
/// with an `errors` array on failure).
pub fn data(resp: &serde_json::Value) -> Result<&serde_json::Value> {
    if let Some(errs) = resp.get("errors").and_then(|e| e.as_array())
        && !errs.is_empty()
    {
        let msg = errs
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(anyhow!("fly graphql error: {msg}"));
    }
    resp.get("data")
        .ok_or_else(|| anyhow!("fly graphql: no data in response: {resp}"))
}

const APP_IPS_QUERY: &str = "\
query($name: String!) {\n\
  app(name: $name) { ipAddresses { nodes { address type } } }\n\
}";

/// Query an app's IP addresses (to reuse an existing v4 on re-create instead of
/// allocating a second one).
pub fn app_ips_query(app: &str) -> serde_json::Value {
    request(APP_IPS_QUERY, serde_json::json!({ "name": app }))
}

/// The first dedicated-IPv4 (`type == "v4"`) already on the app, if any.
pub fn parse_app_ipv4(resp: &serde_json::Value) -> Option<String> {
    data(resp)
        .ok()?
        .pointer("/app/ipAddresses/nodes")?
        .as_array()?
        .iter()
        .find(|n| n.get("type").and_then(|t| t.as_str()) == Some("v4"))
        .and_then(|n| n.get("address").and_then(|a| a.as_str()))
        .map(str::to_string)
}

const ALLOCATE_IPV4_MUTATION: &str = "\
mutation($input: AllocateIPAddressInput!) {\n\
  allocateIpAddress(input: $input) { ipAddress { id address type } }\n\
}";

/// Allocate a dedicated public IPv4 to `app` (`appId` is the app name in Fly's
/// GraphQL). Billed ~$2/mo, prorated hourly, released when the app is deleted.
pub fn allocate_ipv4(app: &str) -> serde_json::Value {
    request(
        ALLOCATE_IPV4_MUTATION,
        serde_json::json!({ "input": { "appId": app, "type": "v4" } }),
    )
}

/// The allocated IPv4 address from an `allocateIpAddress` response.
pub fn parse_allocated_ipv4(resp: &serde_json::Value) -> Result<String> {
    data(resp)?
        .pointer("/allocateIpAddress/ipAddress/address")
        .and_then(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("fly graphql: no allocated IPv4 in response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_surfaces_graphql_errors() {
        let ok = serde_json::json!({ "data": { "x": 1 } });
        assert_eq!(data(&ok).unwrap(), &serde_json::json!({ "x": 1 }));
        let err = serde_json::json!({ "errors": [ { "message": "nope" } ] });
        assert!(data(&err).unwrap_err().to_string().contains("nope"));
    }

    #[test]
    fn app_ips_query_and_parse_picks_v4() {
        let req = app_ips_query("sz-app");
        assert!(req["query"].as_str().unwrap().contains("ipAddresses"));
        assert_eq!(req["variables"]["name"], "sz-app");
        let resp = serde_json::json!({ "data": { "app": { "ipAddresses": { "nodes": [
            { "address": "2a09:8280::1", "type": "v6" },
            { "address": "137.66.60.73", "type": "v4" }
        ]}}}});
        assert_eq!(parse_app_ipv4(&resp).as_deref(), Some("137.66.60.73"));
        // No v4 present ⇒ None (caller allocates one).
        let none = serde_json::json!({ "data": { "app": { "ipAddresses": { "nodes": [] } } } });
        assert!(parse_app_ipv4(&none).is_none());
    }

    #[test]
    fn allocate_ipv4_request_and_parse() {
        let req = allocate_ipv4("sz-app");
        assert!(req["query"].as_str().unwrap().contains("allocateIpAddress"));
        assert_eq!(req["variables"]["input"]["appId"], "sz-app");
        assert_eq!(req["variables"]["input"]["type"], "v4");
        let resp = serde_json::json!({ "data": { "allocateIpAddress": { "ipAddress": {
            "id": "ip_x", "address": "137.66.60.73", "type": "v4"
        }}}});
        assert_eq!(parse_allocated_ipv4(&resp).unwrap(), "137.66.60.73");
        assert!(parse_allocated_ipv4(&serde_json::json!({"data":{}})).is_err());
    }
}
