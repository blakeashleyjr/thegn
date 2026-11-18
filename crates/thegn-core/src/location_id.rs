//! The location-identity discriminator for `worktrees.location` (resolutions
//! B36): v2 JSON blobs carry `"v": 2` and `"kind": "provider" | "connector"`.
//!
//! Stub landed by WS00-P1-06: types and a pure [`peek_kind`], no caller (in
//! particular, [`GitLoc::from_db`](crate::remote::GitLoc::from_db) does not read
//! it yet). Owners after WS00: WS08-P2-01 (provider blobs, persistence, the lazy
//! v1 upgrade) and WS09-P2-03 (connector blobs, `GitLoc::Unresolved`).
//!
//! Rules (B36):
//! - **Transport is derived at read time.** No blob holds an executable path;
//!   `GitLoc::from_db` builds the control prefix from the running binary plus a
//!   self-bridge (`thegn provider-exec …` / `thegn connector exec …`).
//! - **v1 rows stay readable.** A v1 control-prefix row (or an ssh row) has no
//!   `v`/`kind`, so [`peek_kind`] returns `None` and the v1 reader handles it.
//! - **One tripwire exception.** WS08-P2-01 adds to [`ProviderLocV2`] an
//!   optional bare-name `control_prefix` (`["thegn", "provider-exec",
//!   "--provider", R, "--env", E, "--id", I, "--"]`, never an absolute path) so a
//!   build older than WS08-P2-01 routes through `provider-exec` and fails
//!   safely. v2-aware builds never read it, and [`peek_kind`] ignores it: only
//!   the `v` and `kind` keys decide. [`ConnectorLocV2`] has no such field.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The location blob version this module understands.
pub const LOCATION_V2: u8 = 2;

/// Which kind of v2 location a blob describes. Stub (WS00-P1-06); owners
/// WS08-P2-01 (provider) and WS09-P2-03 (connector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocKind {
    /// A provider-placed worktree (built-in, config or plugin provider).
    Provider,
    /// A worktree reached through a connector.
    Connector,
}

impl LocKind {
    /// The `kind` value written in the blob.
    pub const fn as_str(self) -> &'static str {
        match self {
            LocKind::Provider => "provider",
            LocKind::Connector => "connector",
        }
    }

    fn parse(s: &str) -> Option<LocKind> {
        match s {
            "provider" => Some(LocKind::Provider),
            "connector" => Some(LocKind::Connector),
            _ => None,
        }
    }
}

/// The result of [`peek_kind`]: a blob's version and kind. Stub (WS00-P1-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocPeek {
    /// The blob's `v` (always [`LOCATION_V2`] today).
    pub version: u8,
    /// The blob's `kind`.
    pub kind: LocKind,
}

/// Pure: reads only the top-level `v` and `kind` keys of a `worktrees.location`
/// blob. `None` for v1 control-prefix rows, ssh rows, non-JSON, unknown kinds
/// and unknown versions. Stub (WS00-P1-06); no caller yet.
pub fn peek_kind(location: &str) -> Option<LocPeek> {
    let value: serde_json::Value = serde_json::from_str(location.trim()).ok()?;
    let object = value.as_object()?;
    let version = u8::try_from(object.get("v")?.as_u64()?).ok()?;
    if version != LOCATION_V2 {
        return None;
    }
    let kind = LocKind::parse(object.get("kind")?.as_str()?)?;
    Some(LocPeek { version, kind })
}

/// A v2 provider location blob (B36). Stub (WS00-P1-06); owner WS08-P2-01,
/// which adds the optional `control_prefix` tripwire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLocV2 {
    /// Always [`LOCATION_V2`].
    pub v: u8,
    /// Always `"provider"`.
    pub kind: String,
    /// `builtin:<kind>`, `config:<name>` or `plugin:<plugin-id>/<contribution-id>`.
    #[serde(rename = "ref")]
    pub provider_ref: String,
    /// The `[env.<name>]` the worktree was created from.
    pub env: String,
    /// The provider's sandbox id.
    pub id: String,
    /// The provider's handle, when it differs from the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// The account fingerprint the sandbox belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The worktree path inside the environment.
    pub path: String,
}

/// A v2 connector location blob (B36). Stub (WS00-P1-06); owner WS09-P2-03.
/// Never holds a control prefix or an executable path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorLocV2 {
    /// Always [`LOCATION_V2`].
    pub v: u8,
    /// Always `"connector"`.
    pub kind: String,
    /// The `[connector.<name>]` name.
    pub connector: String,
    /// The connector definition's digest at creation.
    pub digest: String,
    /// The connector's template variables.
    pub vars: BTreeMap<String, String>,
    /// The worktree path on the connected host.
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDER_V2: &str = r#"{"v":2,"kind":"provider","ref":"builtin:sprite","env":"sprite","id":"sb-123","handle":"tg-wt","account":"acct-fp","path":"/home/sprite/wt"}"#;
    const CONNECTOR_V2: &str = r#"{"v":2,"kind":"connector","connector":"lab","digest":"sha256-abc","vars":{"host":"lab.internal","user":"dev"},"path":"/srv/wt"}"#;
    const PROVIDER_V2_TRIPWIRE: &str = r#"{"v":2,"kind":"provider","ref":"config:hetzner","env":"hz","id":"vm-9","path":"/root/wt","control_prefix":["thegn","provider-exec","--provider","config:hetzner","--env","hz","--id","vm-9","--"]}"#;
    const PROVIDER_V2_ABSOLUTE_EXE: &str = r#"{"v":2,"kind":"provider","ref":"config:hetzner","env":"hz","id":"vm-9","path":"/root/wt","control_prefix":["/usr/bin/thegn","provider-exec","--provider","config:hetzner","--env","hz","--id","vm-9","--"]}"#;

    /// Whether any string in the blob, other than the top-level in-env `path`,
    /// is an absolute path (an executable path the blob must never carry).
    fn carries_exe_path(blob: &str) -> bool {
        fn walk(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::String(s) => s.starts_with('/') || s.contains("/proc/"),
                serde_json::Value::Array(items) => items.iter().any(walk),
                serde_json::Value::Object(map) => map.values().any(walk),
                _ => false,
            }
        }
        let value: serde_json::Value = serde_json::from_str(blob).unwrap();
        value
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.as_str() != "path")
            .any(|(_, value)| walk(value))
    }

    #[test]
    fn peek_v1_control_prefix_is_none() {
        // The v1 shapes `GitLoc::from_db` reads today: a provider control
        // prefix and an ssh target.
        let v1_provider =
            r#"{"control_prefix":["sprite","exec","-s","tg-wt","--"],"path":"/home/sprite/wt"}"#;
        let v1_ssh = r#"{"host":"box","port":22,"forward_agent":false,"path":"/r/wt"}"#;
        assert_eq!(peek_kind(v1_provider), None);
        assert_eq!(peek_kind(v1_ssh), None);
        assert_eq!(peek_kind("local"), None);
        assert_eq!(peek_kind(""), None);
    }

    #[test]
    fn peek_provider_v2() {
        assert_eq!(
            peek_kind(PROVIDER_V2),
            Some(LocPeek {
                version: 2,
                kind: LocKind::Provider
            })
        );
        assert_eq!(LocKind::Provider.as_str(), "provider");
    }

    #[test]
    fn peek_connector_v2() {
        assert_eq!(
            peek_kind(&format!("  {CONNECTOR_V2}\n")),
            Some(LocPeek {
                version: 2,
                kind: LocKind::Connector
            })
        );
        assert_eq!(LocKind::Connector.as_str(), "connector");
    }

    #[test]
    fn peek_unknown_kind_is_none() {
        assert_eq!(peek_kind(r#"{"v":2,"kind":"wsl","path":"/wt"}"#), None);
        assert_eq!(peek_kind(r#"{"v":2,"kind":7,"path":"/wt"}"#), None);
        assert_eq!(peek_kind(r#"{"v":2,"path":"/wt"}"#), None);
        // Unknown and malformed versions.
        assert_eq!(peek_kind(r#"{"v":3,"kind":"provider"}"#), None);
        assert_eq!(peek_kind(r#"{"v":1,"kind":"provider"}"#), None);
        assert_eq!(peek_kind(r#"{"v":258,"kind":"provider"}"#), None);
        assert_eq!(peek_kind(r#"{"v":"2","kind":"provider"}"#), None);
        assert_eq!(peek_kind(r#"{"kind":"provider"}"#), None);
    }

    #[test]
    fn peek_non_json_is_none() {
        assert_eq!(peek_kind("not json"), None);
        assert_eq!(peek_kind("[2, \"provider\"]"), None);
        assert_eq!(peek_kind("2"), None);
        assert_eq!(peek_kind(r#"{"v":2,"kind":"provider""#), None);
    }

    #[test]
    fn provider_loc_v2_roundtrip() {
        let blob: ProviderLocV2 = serde_json::from_str(PROVIDER_V2).unwrap();
        assert_eq!(blob.provider_ref, "builtin:sprite");
        assert_eq!(blob.handle.as_deref(), Some("tg-wt"));
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::from_str::<serde_json::Value>(PROVIDER_V2).unwrap()
        );
        assert_eq!(serde_json::from_str::<ProviderLocV2>(&json).unwrap(), blob);

        let bare = ProviderLocV2 {
            handle: None,
            account: None,
            ..blob
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("handle") && !json.contains("account"));
        assert_eq!(serde_json::from_str::<ProviderLocV2>(&json).unwrap(), bare);
        assert_eq!(
            peek_kind(&json).map(|peek| peek.kind),
            Some(LocKind::Provider)
        );
    }

    #[test]
    fn connector_loc_v2_roundtrip() {
        let blob: ConnectorLocV2 = serde_json::from_str(CONNECTOR_V2).unwrap();
        assert_eq!(
            blob.vars.get("host").map(String::as_str),
            Some("lab.internal")
        );
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(serde_json::from_str::<ConnectorLocV2>(&json).unwrap(), blob);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::from_str::<serde_json::Value>(CONNECTOR_V2).unwrap()
        );
        assert!(!json.contains("control_prefix"));
    }

    #[test]
    fn blobs_carry_no_exe_path() {
        assert!(!carries_exe_path(PROVIDER_V2));
        assert!(!carries_exe_path(CONNECTOR_V2));
        assert!(!carries_exe_path(PROVIDER_V2_TRIPWIRE));
        assert!(carries_exe_path(PROVIDER_V2_ABSOLUTE_EXE));
    }

    #[test]
    fn peek_ignores_tripwire_control_prefix() {
        let peek = peek_kind(PROVIDER_V2_TRIPWIRE).unwrap();
        assert_eq!(peek.kind, LocKind::Provider);
        assert_eq!(peek.version, LOCATION_V2);
        // The WS00 type does not carry the tripwire (WS08-P2-01 adds it), and
        // reading a tripwire blob ignores it.
        let blob: ProviderLocV2 = serde_json::from_str(PROVIDER_V2_TRIPWIRE).unwrap();
        assert_eq!(blob.id, "vm-9");
    }
}
