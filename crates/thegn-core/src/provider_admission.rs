//! Bounded, provider-independent admission for the static part of managed
//! provider specifications.
//!
//! This module deliberately does not probe a vendor or read credentials. It
//! owns only the syntax and local safety checks which must be shared by strict
//! configuration validation and the service constructors immediately before a
//! provider can create anything.

use std::collections::BTreeMap;
use std::fmt;

pub const MAX_PROVIDER_NAME_BYTES: usize = 128;
pub const MAX_PROVIDER_FIELD_BYTES: usize = 128;
pub const MAX_PROVIDER_IMAGE_BYTES: usize = 512;
pub const MAX_PROVIDER_ENDPOINT_BYTES: usize = 2_048;
pub const MAX_PROVIDER_KEY_BYTES: usize = 8 * 1024;
pub const MAX_PROVIDER_INSTANCES: u32 = 256;
pub const MAX_PROVIDER_METADATA_ENTRIES: usize = 8;
pub const MAX_PROVIDER_METADATA_KEY_BYTES: usize = 64;
pub const MAX_PROVIDER_METADATA_VALUE_BYTES: usize = 256;

/// A validation error contains only stable field/reason labels. It never
/// includes the rejected value, an endpoint, or a credential.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ProviderAdmissionError {
    pub field: &'static str,
    pub reason: &'static str,
}

impl ProviderAdmissionError {
    const fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

impl fmt::Debug for ProviderAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderAdmissionError")
            .field("field", &self.field)
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for ProviderAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ProviderAdmissionError {}

/// The one local Fly guest vocabulary. Vendor availability is intentionally
/// not inferred here; these are the request shapes the adapter knows how to
/// render.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlySize {
    SharedCpu1x,
    SharedCpu2x,
    SharedCpu4x,
    SharedCpu8x,
    Performance1x,
    Performance2x,
    Performance4x,
}

impl FlySize {
    pub fn parse(raw: &str) -> Result<Self, ProviderAdmissionError> {
        match raw {
            "shared-cpu-1x" => Ok(Self::SharedCpu1x),
            "shared-cpu-2x" => Ok(Self::SharedCpu2x),
            "shared-cpu-4x" => Ok(Self::SharedCpu4x),
            "shared-cpu-8x" => Ok(Self::SharedCpu8x),
            "performance-1x" => Ok(Self::Performance1x),
            "performance-2x" => Ok(Self::Performance2x),
            "performance-4x" => Ok(Self::Performance4x),
            _ => Err(ProviderAdmissionError::new(
                "size",
                "unknown Fly guest preset",
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SharedCpu1x => "shared-cpu-1x",
            Self::SharedCpu2x => "shared-cpu-2x",
            Self::SharedCpu4x => "shared-cpu-4x",
            Self::SharedCpu8x => "shared-cpu-8x",
            Self::Performance1x => "performance-1x",
            Self::Performance2x => "performance-2x",
            Self::Performance4x => "performance-4x",
        }
    }

    pub const fn guest(self) -> (&'static str, u8, u16) {
        match self {
            Self::SharedCpu1x => ("shared", 1, 256),
            Self::SharedCpu2x => ("shared", 2, 512),
            Self::SharedCpu4x => ("shared", 4, 1024),
            Self::SharedCpu8x => ("shared", 8, 2048),
            Self::Performance1x => ("performance", 1, 2048),
            Self::Performance2x => ("performance", 2, 4096),
            Self::Performance4x => ("performance", 4, 8192),
        }
    }
}

impl fmt::Debug for FlySize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum FlyImage {
    Default,
    Reference(String),
}

impl FlyImage {
    pub fn resolve<'a>(&'a self, default: &'a str) -> &'a str {
        match self {
            Self::Default => default,
            Self::Reference(reference) => reference,
        }
    }
}

impl fmt::Debug for FlyImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Reference(_) => f.write_str("reference(<redacted>)"),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum VpsImage {
    Default,
    Image(String),
    Snapshot(String),
}

impl VpsImage {
    pub fn resolve<'a>(&'a self, default: &'a str) -> &'a str {
        match self {
            Self::Default => default,
            Self::Image(image) | Self::Snapshot(image) => image,
        }
    }

    pub fn is_snapshot(&self) -> bool {
        matches!(self, Self::Snapshot(_))
    }
}

impl fmt::Debug for VpsImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Image(_) => f.write_str("image(<redacted>)"),
            Self::Snapshot(_) => f.write_str("snapshot(<redacted>)"),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FlyStaticSpec {
    pub api_base: String,
    pub graphql_url: String,
    pub org_slug: String,
    pub name: Option<String>,
    pub region: String,
    pub size: FlySize,
    pub image: FlyImage,
    pub max_instances: u32,
    pub max_lifetime_secs: u64,
    pub pubkey: Option<String>,
}

impl fmt::Debug for FlyStaticSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlyStaticSpec")
            .field("api_base", &"<redacted>")
            .field("graphql_url", &"<redacted>")
            .field("org_slug", &"<redacted>")
            .field("name", &self.name.as_ref().map(|_| "<redacted>"))
            .field("region", &"<redacted>")
            .field("size", &self.size)
            .field("image", &self.image)
            .field("max_instances", &self.max_instances)
            .field("max_lifetime_secs", &self.max_lifetime_secs)
            .field("pubkey", &self.pubkey.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct VpsStaticSpec {
    pub kind: String,
    pub api_base: String,
    pub name: Option<String>,
    pub region: String,
    pub size: String,
    pub image: VpsImage,
    pub max_instances: u32,
    pub max_lifetime_secs: u64,
    pub pubkey: Option<String>,
}

impl fmt::Debug for VpsStaticSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VpsStaticSpec")
            .field("kind", &self.kind)
            .field("api_base", &"<redacted>")
            .field("name", &self.name.as_ref().map(|_| "<redacted>"))
            .field("region", &"<redacted>")
            .field("size", &"<redacted>")
            .field("image", &self.image)
            .field("max_instances", &self.max_instances)
            .field("max_lifetime_secs", &self.max_lifetime_secs)
            .field("pubkey", &self.pubkey.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

pub struct FlyInput<'a> {
    pub api_base: &'a str,
    pub graphql_url: &'a str,
    pub org_slug: &'a str,
    pub name: Option<&'a str>,
    pub region: &'a str,
    pub size: &'a str,
    pub image: &'a str,
    pub max_instances: u32,
    pub max_lifetime_secs: u64,
    pub pubkey: Option<&'a str>,
    pub metadata: Option<&'a BTreeMap<String, String>>,
}

pub struct VpsInput<'a> {
    pub kind: &'a str,
    pub api_base: &'a str,
    pub name: Option<&'a str>,
    pub region: &'a str,
    pub size: &'a str,
    pub image: &'a str,
    pub max_instances: u32,
    pub max_lifetime_secs: u64,
    pub pubkey: Option<&'a str>,
}

pub fn admit_fly(input: FlyInput<'_>) -> Result<FlyStaticSpec, ProviderAdmissionError> {
    let name = input
        .name
        .map(|value| required_text("name", value, MAX_PROVIDER_NAME_BYTES))
        .transpose()?;
    let pubkey = input.pubkey.map(validate_pubkey).transpose()?;
    let size = if input.size.is_empty() {
        "shared-cpu-2x".to_owned()
    } else {
        bounded_nonempty("size", input.size, MAX_PROVIDER_FIELD_BYTES)?
    };
    if let Some(metadata) = input.metadata {
        validate_metadata(metadata)?;
    }
    Ok(FlyStaticSpec {
        api_base: endpoint("api_base", input.api_base)?,
        graphql_url: endpoint("graphql_url", input.graphql_url)?,
        org_slug: optional_text("org", input.org_slug, "personal", MAX_PROVIDER_FIELD_BYTES)?,
        name,
        region: optional_text("region", input.region, "iad", MAX_PROVIDER_FIELD_BYTES)?,
        size: FlySize::parse(&size)?,
        image: fly_image(input.image)?,
        max_instances: instance_cap(input.max_instances)?,
        max_lifetime_secs: lifetime(input.max_lifetime_secs)?,
        pubkey,
    })
}

pub fn admit_vps(input: VpsInput<'_>) -> Result<VpsStaticSpec, ProviderAdmissionError> {
    if input.kind != "hetzner" && input.kind != "digitalocean" {
        return Err(ProviderAdmissionError::new(
            "provider",
            "provider kind is not implemented by the VPS adapter",
        ));
    }
    let name = input
        .name
        .map(|value| required_text("name", value, MAX_PROVIDER_NAME_BYTES))
        .transpose()?;
    let pubkey = input.pubkey.map(validate_pubkey).transpose()?;
    Ok(VpsStaticSpec {
        kind: input.kind.to_owned(),
        api_base: endpoint("api_base", input.api_base)?,
        name,
        region: optional_text("region", input.region, "", MAX_PROVIDER_FIELD_BYTES)?,
        size: optional_text("size", input.size, "", MAX_PROVIDER_FIELD_BYTES)?,
        image: vps_image(input.image)?,
        max_instances: instance_cap(input.max_instances)?,
        max_lifetime_secs: lifetime(input.max_lifetime_secs)?,
        pubkey,
    })
}

pub fn validate_fly_config(
    api_base: &str,
    region: &str,
    size: &str,
    image: &str,
    max_instances: u32,
    max_lifetime_secs: u64,
) -> Result<(), ProviderAdmissionError> {
    admit_fly(FlyInput {
        api_base,
        graphql_url: "",
        org_slug: "",
        name: None,
        region,
        size,
        image,
        max_instances,
        max_lifetime_secs,
        pubkey: None,
        metadata: None,
    })
    .map(|_| ())
}

pub fn validate_vps_config(
    kind: &str,
    api_base: &str,
    region: &str,
    size: &str,
    image: &str,
    max_instances: u32,
    max_lifetime_secs: u64,
) -> Result<(), ProviderAdmissionError> {
    admit_vps(VpsInput {
        kind,
        api_base,
        name: None,
        region,
        size,
        image,
        max_instances,
        max_lifetime_secs,
        pubkey: None,
    })
    .map(|_| ())
}

/// Validate every configured Fly/VPS table using the same local grammar that
/// the service constructors use. Provider names outside these implemented
/// adapters are handled by the schema/enum validator and are not probed here.
pub fn validate_config(cfg: &crate::config::Config) -> Vec<String> {
    let mut errors = Vec::new();
    for (env_name, env) in &cfg.env {
        let provider = env.provider.provider.trim().to_ascii_lowercase();
        let result = match provider.as_str() {
            "fly" => validate_fly_config(
                &env.provider.api_base,
                &env.provider.region,
                &env.provider.size,
                &env.provider.template,
                env.provider.max_instances,
                env.provider.max_lifetime_secs,
            ),
            "hetzner" | "digitalocean" => validate_vps_config(
                provider.as_str(),
                &env.provider.api_base,
                &env.provider.region,
                &env.provider.size,
                &env.provider.template,
                env.provider.max_instances,
                env.provider.max_lifetime_secs,
            ),
            _ => Ok(()),
        };
        if let Err(error) = result {
            errors.push(format!(
                "env.{env_name}.provider.{}: {}",
                error.field, error.reason
            ));
        }
    }
    errors
}

fn endpoint(field: &'static str, raw: &str) -> Result<String, ProviderAdmissionError> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let value = bounded_nonempty(field, raw, MAX_PROVIDER_ENDPOINT_BYTES)?;
    let Some((scheme, authority_and_path)) = value.split_once("://") else {
        return Err(ProviderAdmissionError::new(
            field,
            "endpoint must use http or https",
        ));
    };
    if scheme != "http" && scheme != "https" {
        return Err(ProviderAdmissionError::new(
            field,
            "endpoint must use http or https",
        ));
    }
    if authority_and_path.contains('#') {
        return Err(ProviderAdmissionError::new(
            field,
            "endpoint fragments are not allowed",
        ));
    }
    let authority_end = authority_and_path
        .find(['/', '?'])
        .unwrap_or(authority_and_path.len());
    let authority = &authority_and_path[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(ProviderAdmissionError::new(
            field,
            "endpoint host is missing or has credentials",
        ));
    }
    let (host, port) = if authority.starts_with('[') {
        let Some(close) = authority.find(']') else {
            return Err(ProviderAdmissionError::new(
                field,
                "endpoint IPv6 host is malformed",
            ));
        };
        let host = &authority[1..close];
        let suffix = &authority[close + 1..];
        if host.is_empty()
            || suffix == ":"
            || suffix.chars().any(|c| c != ':' && !c.is_ascii_digit())
        {
            return Err(ProviderAdmissionError::new(
                field,
                "endpoint port is malformed",
            ));
        }
        (host, suffix.strip_prefix(':').unwrap_or(""))
    } else {
        let colon_count = authority.bytes().filter(|byte| *byte == b':').count();
        if colon_count > 1 {
            return Err(ProviderAdmissionError::new(
                field,
                "IPv6 endpoints must use brackets",
            ));
        }
        if authority.ends_with(':') {
            return Err(ProviderAdmissionError::new(
                field,
                "endpoint port is malformed",
            ));
        }
        authority.split_once(':').unwrap_or((authority, ""))
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_:".contains(c))
    {
        return Err(ProviderAdmissionError::new(
            field,
            "endpoint host is malformed",
        ));
    }
    if !port.is_empty() {
        let Ok(port) = port.parse::<u16>() else {
            return Err(ProviderAdmissionError::new(
                field,
                "endpoint port is malformed",
            ));
        };
        if port == 0 {
            return Err(ProviderAdmissionError::new(
                field,
                "endpoint port must be nonzero",
            ));
        }
    }
    Ok(value.to_owned())
}

fn required_text(
    field: &'static str,
    raw: &str,
    max: usize,
) -> Result<String, ProviderAdmissionError> {
    if raw.is_empty() {
        return Err(ProviderAdmissionError::new(field, "value is required"));
    }
    bounded_nonempty(field, raw, max)
}

fn optional_text(
    field: &'static str,
    raw: &str,
    default: &str,
    max: usize,
) -> Result<String, ProviderAdmissionError> {
    if raw.is_empty() {
        return Ok(default.to_owned());
    }
    bounded_nonempty(field, raw, max)
}

fn bounded_nonempty(
    field: &'static str,
    raw: &str,
    max: usize,
) -> Result<String, ProviderAdmissionError> {
    if raw.len() > max {
        return Err(ProviderAdmissionError::new(
            field,
            "value exceeds its bounded length",
        ));
    }
    if raw.trim() != raw || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ProviderAdmissionError::new(
            field,
            "value contains whitespace or control characters",
        ));
    }
    Ok(raw.to_owned())
}

fn fly_image(raw: &str) -> Result<FlyImage, ProviderAdmissionError> {
    if raw.is_empty() {
        return Ok(FlyImage::Default);
    }
    if raw.starts_with("snapshot:") {
        return Err(ProviderAdmissionError::new(
            "image",
            "Fly does not support snapshot image forms",
        ));
    }
    let reference = raw.strip_prefix("image:").unwrap_or(raw);
    if reference.is_empty() || reference.starts_with("image:") {
        return Err(ProviderAdmissionError::new(
            "image",
            "image reference is malformed",
        ));
    }
    Ok(FlyImage::Reference(bounded_nonempty(
        "image",
        reference,
        MAX_PROVIDER_IMAGE_BYTES,
    )?))
}

fn vps_image(raw: &str) -> Result<VpsImage, ProviderAdmissionError> {
    if raw.is_empty() {
        return Ok(VpsImage::Default);
    }
    if let Some(id) = raw.strip_prefix("snapshot:") {
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ProviderAdmissionError::new(
                "image",
                "snapshot id is malformed",
            ));
        }
        let Ok(value) = id.parse::<i64>() else {
            return Err(ProviderAdmissionError::new(
                "image",
                "snapshot id is out of range",
            ));
        };
        if value <= 0 {
            return Err(ProviderAdmissionError::new(
                "image",
                "snapshot id must be positive",
            ));
        }
        return Ok(VpsImage::Snapshot(value.to_string()));
    }
    Ok(VpsImage::Image(bounded_nonempty(
        "image",
        raw,
        MAX_PROVIDER_IMAGE_BYTES,
    )?))
}

fn instance_cap(value: u32) -> Result<u32, ProviderAdmissionError> {
    if value > MAX_PROVIDER_INSTANCES {
        Err(ProviderAdmissionError::new(
            "max_instances",
            "value exceeds the provider safety bound",
        ))
    } else {
        Ok(value)
    }
}

fn lifetime(value: u64) -> Result<u64, ProviderAdmissionError> {
    if value > crate::time_policy::MAX_DURATION_SECS {
        Err(ProviderAdmissionError::new(
            "max_lifetime_secs",
            "value exceeds the duration safety bound",
        ))
    } else {
        Ok(value)
    }
}

fn validate_pubkey(raw: &str) -> Result<String, ProviderAdmissionError> {
    if raw.is_empty() || raw.len() > MAX_PROVIDER_KEY_BYTES || raw.chars().any(char::is_control) {
        return Err(ProviderAdmissionError::new(
            "pubkey",
            "public key shape is invalid",
        ));
    }
    let mut fields = raw.split_whitespace();
    let Some(kind) = fields.next() else {
        return Err(ProviderAdmissionError::new(
            "pubkey",
            "public key shape is invalid",
        ));
    };
    let Some(blob) = fields.next() else {
        return Err(ProviderAdmissionError::new(
            "pubkey",
            "public key shape is invalid",
        ));
    };
    let valid_kind = matches!(
        kind,
        "ssh-ed25519"
            | "ssh-rsa"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
            | "sk-ssh-ed25519@openssh.com"
            | "sk-ecdsa-sha2-nistp256@openssh.com"
    );
    if !valid_kind
        || blob.len() < 4
        || !blob
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
    {
        return Err(ProviderAdmissionError::new(
            "pubkey",
            "public key shape is invalid",
        ));
    }
    Ok(raw.to_owned())
}

pub fn validate_metadata(
    metadata: &BTreeMap<String, String>,
) -> Result<(), ProviderAdmissionError> {
    if metadata.len() > MAX_PROVIDER_METADATA_ENTRIES {
        return Err(ProviderAdmissionError::new(
            "metadata",
            "too many metadata entries",
        ));
    }
    for (key, value) in metadata {
        if key.is_empty()
            || key.len() > MAX_PROVIDER_METADATA_KEY_BYTES
            || value.len() > MAX_PROVIDER_METADATA_VALUE_BYTES
            || key.chars().any(|c| c.is_control() || c.is_whitespace())
            || value.chars().any(char::is_control)
        {
            return Err(ProviderAdmissionError::new(
                "metadata",
                "metadata shape is invalid",
            ));
        }
    }
    if metadata.get("managed-by").map(String::as_str) != Some("thegn")
        || metadata.get("tg-host").is_none_or(String::is_empty)
    {
        return Err(ProviderAdmissionError::new(
            "metadata",
            "required management metadata is missing",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fly(size: &str, image: &str) -> Result<FlyStaticSpec, ProviderAdmissionError> {
        admit_fly(FlyInput {
            api_base: "http://127.0.0.1:8080/v1",
            graphql_url: "http://127.0.0.1:8080/graphql",
            org_slug: "",
            name: Some("sandbox-1"),
            region: "",
            size,
            image,
            max_instances: 0,
            max_lifetime_secs: 0,
            pubkey: Some("ssh-ed25519 MOCKKEY thegn"),
            metadata: None,
        })
    }

    #[test]
    fn fly_sizes_are_shared_and_unknown_is_typed() {
        assert_eq!(FlySize::parse("shared-cpu-2x").unwrap().guest().2, 512);
        assert_eq!(
            fly("shared-cpu-4x", "").unwrap().size.as_str(),
            "shared-cpu-4x"
        );
        assert_eq!(FlySize::parse("typo").unwrap_err().field, "size");
    }

    #[test]
    fn malformed_provider_values_do_not_echo_values_or_secrets() {
        let error = fly("shared-cpu-2x", "snapshot:42").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("image"));
        assert!(!text.contains("42"));
        assert!(!format!("{error:?}").contains("MOCKKEY"));
    }

    #[test]
    fn endpoints_require_schemes_and_reject_credentials_fragments_and_zero_ports() {
        for endpoint in [
            "127.0.0.1:8080",
            "https://user:token@example.invalid/api",
            "https://example.invalid/api#fragment",
            "http://127.0.0.1:0/api",
        ] {
            let error = admit_fly(FlyInput {
                api_base: endpoint,
                graphql_url: "",
                org_slug: "",
                name: None,
                region: "",
                size: "",
                image: "",
                max_instances: 0,
                max_lifetime_secs: 0,
                pubkey: None,
                metadata: None,
            })
            .unwrap_err();
            assert_eq!(error.field, "api_base");
        }
    }

    #[test]
    fn vps_snapshot_is_numeric_and_never_falls_back_to_an_image() {
        let valid = admit_vps(VpsInput {
            kind: "hetzner",
            api_base: "",
            name: None,
            region: "",
            size: "",
            image: "snapshot:42",
            max_instances: 0,
            max_lifetime_secs: 0,
            pubkey: None,
        })
        .unwrap();
        assert!(valid.image.is_snapshot());
        for image in ["snapshot:", "snapshot:nope", "snapshot:9223372036854775808"] {
            assert_eq!(
                admit_vps(VpsInput {
                    kind: "hetzner",
                    api_base: "",
                    name: None,
                    region: "",
                    size: "",
                    image,
                    max_instances: 0,
                    max_lifetime_secs: 0,
                    pubkey: None,
                })
                .unwrap_err()
                .field,
                "image"
            );
        }
    }

    #[test]
    fn provider_bounds_accept_exact_limits_and_reject_one_over() {
        let exact_name = "n".repeat(MAX_PROVIDER_NAME_BYTES);
        let exact = admit_vps(VpsInput {
            kind: "hetzner",
            api_base: "",
            name: Some(&exact_name),
            region: "",
            size: "",
            image: "",
            max_instances: MAX_PROVIDER_INSTANCES,
            max_lifetime_secs: crate::time_policy::MAX_DURATION_SECS,
            pubkey: None,
        });
        assert!(exact.is_ok());

        let over_name = "n".repeat(MAX_PROVIDER_NAME_BYTES + 1);
        let over = admit_vps(VpsInput {
            kind: "hetzner",
            api_base: "",
            name: Some(&over_name),
            region: "",
            size: "",
            image: "",
            max_instances: MAX_PROVIDER_INSTANCES + 1,
            max_lifetime_secs: crate::time_policy::MAX_DURATION_SECS,
            pubkey: None,
        });
        assert_eq!(over.unwrap_err().field, "name");
        assert_eq!(
            validate_vps_config("hetzner", "", "", "", "", MAX_PROVIDER_INSTANCES + 1, 0)
                .unwrap_err()
                .field,
            "max_instances"
        );
    }
}
