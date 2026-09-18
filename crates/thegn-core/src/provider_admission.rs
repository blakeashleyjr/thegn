//! Bounded, provider-independent admission for the static part of managed
//! provider specifications.
//!
//! This module deliberately does not probe a vendor or read credentials. It
//! owns only the local grammar and safety checks which must be shared by
//! strict configuration validation and the service constructors immediately
//! before a provider can create anything. A value accepted here is merely
//! well-formed: it proves nothing about vendor availability, quota, account
//! membership, or whether an image exists.
//!
//! Every check runs on borrowed input before any owned copy is made, and every
//! error carries only a stable field/reason label — never the rejected value,
//! an endpoint, a token, or key material.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// Provider sandbox names become vendor server/app names and SSH host aliases,
/// so they follow the portable RFC 1123 hostname subset.
pub const MAX_PROVIDER_NAME_BYTES: usize = 63;
/// Region, organization, and size slugs.
pub const MAX_PROVIDER_FIELD_BYTES: usize = 64;
pub const MAX_PROVIDER_IMAGE_BYTES: usize = 512;
pub const MAX_PROVIDER_ENDPOINT_BYTES: usize = 2_048;
pub const MAX_PROVIDER_KEY_BYTES: usize = 8 * 1024;
pub const MAX_PROVIDER_INSTANCES: u32 = 256;
pub const MAX_PROVIDER_METADATA_ENTRIES: usize = 8;
pub const MAX_PROVIDER_METADATA_KEY_BYTES: usize = 64;
pub const MAX_PROVIDER_METADATA_VALUE_BYTES: usize = 256;
pub const MAX_PROVIDER_INJECTION_BYTES: usize = 256;

const DEFAULT_FLY_REGION: &str = "iad";
const DEFAULT_FLY_ORG: &str = "personal";
const MAX_DNS_NAME_BYTES: usize = 253;
const MAX_DNS_LABEL_BYTES: usize = 63;
const MAX_IMAGE_NAME_BYTES: usize = 255;
const MAX_IMAGE_TAG_BYTES: usize = 128;
const MAX_VPS_IMAGE_BYTES: usize = 128;
const MAX_SNAPSHOT_DIGITS: usize = 19;
const MAX_RSA_MODULUS_BYTES: usize = 2_048;
const MIN_RSA_MODULUS_BYTES: usize = 256;

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
    pub const DEFAULT: Self = Self::SharedCpu2x;

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
    /// A validated OCI reference. `prebaked` records the documented `image:`
    /// prefix, which selects a thegn-built image running its own sshd.
    Reference {
        reference: String,
        prebaked: bool,
    },
}

impl FlyImage {
    pub fn resolve<'a>(&'a self, default: &'a str) -> &'a str {
        match self {
            Self::Default => default,
            Self::Reference { reference, .. } => reference,
        }
    }

    pub fn is_prebaked(&self) -> bool {
        matches!(self, Self::Reference { prebaked: true, .. })
    }
}

impl fmt::Debug for FlyImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Reference { prebaked, .. } => f
                .debug_struct("reference")
                .field("value", &"<redacted>")
                .field("prebaked", prebaked)
                .finish(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VpsKind {
    Hetzner,
    DigitalOcean,
}

impl VpsKind {
    pub fn parse(raw: &str) -> Result<Self, ProviderAdmissionError> {
        match raw {
            "hetzner" => Ok(Self::Hetzner),
            "digitalocean" => Ok(Self::DigitalOcean),
            _ => Err(ProviderAdmissionError::new(
                "provider",
                "provider kind is not implemented by the VPS adapter",
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hetzner => "hetzner",
            Self::DigitalOcean => "digitalocean",
        }
    }
}

/// Admitted Fly inputs. Empty endpoint/optional fields mean "the adapter's
/// documented default"; supplied values are exactly the validated input.
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

/// Admitted VPS inputs. `None` region/size means the vendor shaper's default.
#[derive(Clone, PartialEq, Eq)]
pub struct VpsStaticSpec {
    pub kind: VpsKind,
    pub api_base: String,
    pub name: Option<String>,
    pub region: Option<String>,
    pub size: Option<String>,
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
            .field("region", &self.region.as_ref().map(|_| "<redacted>"))
            .field("size", &self.size.as_ref().map(|_| "<redacted>"))
            .field("image", &self.image)
            .field("max_instances", &self.max_instances)
            .field("max_lifetime_secs", &self.max_lifetime_secs)
            .field("pubkey", &self.pubkey.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Borrowed iroh call-home values injected into a Fly machine environment.
pub struct IrohInjectionInput<'a> {
    pub home_node: &'a str,
    pub sandbox_auth: &'a str,
    pub sandbox_id: &'a str,
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
    pub iroh: Option<IrohInjectionInput<'a>>,
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
    pub metadata: Option<&'a BTreeMap<String, String>>,
}

/// Admit every Fly input. All borrowed checks complete before the first owned
/// copy is made, so a rejected value is never cloned into a request.
pub fn admit_fly(input: FlyInput<'_>) -> Result<FlyStaticSpec, ProviderAdmissionError> {
    if let Some(name) = input.name {
        check_name(name)?;
    }
    if let Some(pubkey) = input.pubkey {
        check_pubkey(pubkey)?;
    }
    if let Some(metadata) = input.metadata {
        validate_metadata(metadata)?;
    }
    if let Some(iroh) = &input.iroh {
        check_iroh(iroh, input.name)?;
    }
    check_endpoint("api_base", input.api_base)?;
    check_endpoint("graphql_url", input.graphql_url)?;
    if !input.org_slug.is_empty() {
        check_slug("org", input.org_slug, MAX_PROVIDER_FIELD_BYTES, false)?;
    }
    if !input.region.is_empty() {
        check_region(input.region)?;
    }
    let size = match input.size {
        "" | "auto" => FlySize::DEFAULT,
        raw => FlySize::parse(raw)?,
    };
    let image = fly_image(input.image)?;
    let max_instances = instance_cap(input.max_instances)?;
    let max_lifetime_secs = lifetime(input.max_lifetime_secs)?;
    Ok(FlyStaticSpec {
        api_base: input.api_base.to_owned(),
        graphql_url: input.graphql_url.to_owned(),
        org_slug: if input.org_slug.is_empty() {
            DEFAULT_FLY_ORG.to_owned()
        } else {
            input.org_slug.to_owned()
        },
        name: input.name.map(str::to_owned),
        region: if input.region.is_empty() {
            DEFAULT_FLY_REGION.to_owned()
        } else {
            input.region.to_owned()
        },
        size,
        image,
        max_instances,
        max_lifetime_secs,
        pubkey: input.pubkey.map(str::to_owned),
    })
}

pub fn admit_vps(input: VpsInput<'_>) -> Result<VpsStaticSpec, ProviderAdmissionError> {
    let kind = VpsKind::parse(input.kind)?;
    if let Some(name) = input.name {
        check_name(name)?;
    }
    if let Some(pubkey) = input.pubkey {
        check_pubkey(pubkey)?;
    }
    if let Some(metadata) = input.metadata {
        validate_metadata(metadata)?;
    }
    check_endpoint("api_base", input.api_base)?;
    if !input.region.is_empty() {
        check_region(input.region)?;
    }
    let size = match input.size {
        "" | "auto" => None,
        raw => {
            check_slug("size", raw, MAX_PROVIDER_FIELD_BYTES, false)?;
            Some(raw)
        }
    };
    let image = vps_image(input.image)?;
    let max_instances = instance_cap(input.max_instances)?;
    let max_lifetime_secs = lifetime(input.max_lifetime_secs)?;
    Ok(VpsStaticSpec {
        kind,
        api_base: input.api_base.to_owned(),
        name: input.name.map(str::to_owned),
        region: (!input.region.is_empty()).then(|| input.region.to_owned()),
        size: size.map(str::to_owned),
        image,
        max_instances,
        max_lifetime_secs,
        pubkey: input.pubkey.map(str::to_owned),
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
        iroh: None,
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
        metadata: None,
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
        let key_ok = !key.is_empty()
            && key.len() <= MAX_PROVIDER_METADATA_KEY_BYTES
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        let value_ok = !value.is_empty()
            && value.len() <= MAX_PROVIDER_METADATA_VALUE_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !key_ok || !value_ok {
            return Err(ProviderAdmissionError::new(
                "metadata",
                "metadata shape is invalid",
            ));
        }
    }
    if metadata.get("managed-by").map(String::as_str) != Some("thegn")
        || !metadata.contains_key("tg-host")
    {
        return Err(ProviderAdmissionError::new(
            "metadata",
            "required management metadata is missing",
        ));
    }
    Ok(())
}

fn err(field: &'static str, reason: &'static str) -> ProviderAdmissionError {
    ProviderAdmissionError::new(field, reason)
}

/// RFC 1123 hostname subset: dot-separated labels of ASCII letters, digits and
/// inner hyphens, at most 63 bytes overall.
fn check_name(raw: &str) -> Result<(), ProviderAdmissionError> {
    if raw.is_empty() {
        return Err(err("name", "value is required"));
    }
    if raw.len() > MAX_PROVIDER_NAME_BYTES {
        return Err(err("name", "value exceeds its bounded length"));
    }
    if !raw.split('.').all(dns_label) {
        return Err(err("name", "name is not a valid host-name label sequence"));
    }
    Ok(())
}

fn dns_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_DNS_LABEL_BYTES
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

/// Lowercase vendor slug: `[a-z0-9]` start/end with inner `-` (and `.` when
/// `allow_dot`), bounded.
fn check_slug(
    field: &'static str,
    raw: &str,
    max: usize,
    allow_dot: bool,
) -> Result<(), ProviderAdmissionError> {
    if raw.len() > max {
        return Err(err(field, "value exceeds its bounded length"));
    }
    let bytes = raw.as_bytes();
    let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let valid = !bytes.is_empty()
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|byte| edge(*byte) || *byte == b'-' || (allow_dot && *byte == b'.'));
    if valid {
        Ok(())
    } else {
        Err(err(field, "value is not a valid lowercase slug"))
    }
}

/// Regions are lowercase slugs that begin with a letter (`iad`, `fsn1`,
/// `nyc3`). Membership in a live region catalog is deliberately not checked.
fn check_region(raw: &str) -> Result<(), ProviderAdmissionError> {
    check_slug("region", raw, 32, false)?;
    if raw.as_bytes()[0].is_ascii_lowercase() {
        Ok(())
    } else {
        Err(err("region", "value is not a valid lowercase slug"))
    }
}

/// Validate an HTTP(S) API endpoint: exact lowercase scheme, a DNS/IPv4/
/// bracketed-IPv6 host, an optional nonzero port, and an optional path of
/// RFC 3986 path characters. Credentials, queries, fragments, backslashes,
/// whitespace and control characters are refused. Empty means "default".
fn check_endpoint(field: &'static str, raw: &str) -> Result<(), ProviderAdmissionError> {
    if raw.is_empty() {
        return Ok(());
    }
    if raw.len() > MAX_PROVIDER_ENDPOINT_BYTES {
        return Err(err(field, "value exceeds its bounded length"));
    }
    if !raw.bytes().all(|byte| byte.is_ascii_graphic()) || raw.contains('\\') {
        return Err(err(field, "endpoint contains a forbidden character"));
    }
    let rest = if let Some(rest) = raw.strip_prefix("https://") {
        rest
    } else if let Some(rest) = raw.strip_prefix("http://") {
        rest
    } else {
        return Err(err(field, "endpoint must use http or https"));
    };
    if rest.contains(['#', '?']) {
        return Err(err(field, "endpoint queries and fragments are not allowed"));
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains(['@', '%']) {
        return Err(err(field, "endpoint host is missing or has credentials"));
    }
    let port = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return Err(err(field, "endpoint IPv6 host is malformed"));
        };
        if host.parse::<Ipv6Addr>().is_err() {
            return Err(err(field, "endpoint IPv6 host is malformed"));
        }
        match suffix {
            "" => None,
            _ => Some(
                suffix
                    .strip_prefix(':')
                    .ok_or(err(field, "endpoint port is malformed"))?,
            ),
        }
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if !valid_host(host) {
            return Err(err(field, "endpoint host is malformed"));
        }
        port
    };
    if let Some(port) = port {
        let valid = !port.is_empty()
            && port.len() <= 5
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port != 0);
        if !valid {
            return Err(err(field, "endpoint port is malformed"));
        }
    }
    if !valid_path(path) {
        return Err(err(field, "endpoint path is malformed"));
    }
    Ok(())
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_DNS_NAME_BYTES {
        return false;
    }
    // A host made only of digits and dots is an IPv4 literal and must parse as
    // one; `999.1.1.1` is not silently reinterpreted as a DNS name.
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return host.parse::<Ipv4Addr>().is_ok();
    }
    host.split('.').all(dns_label)
}

fn valid_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let valid = bytes
                .get(index + 1..index + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
            if !valid {
                return false;
            }
            index += 3;
            continue;
        }
        let allowed = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
                    | b'/'
            );
        if !allowed {
            return false;
        }
        index += 1;
    }
    true
}

/// Fly accepts a bare OCI reference or the documented `image:<reference>`
/// prebaked form. Snapshot forms are a VPS concept and are refused.
fn fly_image(raw: &str) -> Result<FlyImage, ProviderAdmissionError> {
    if raw.is_empty() {
        return Ok(FlyImage::Default);
    }
    if raw.starts_with("snapshot:") {
        return Err(err("image", "Fly does not support snapshot image forms"));
    }
    let (reference, prebaked) = match raw.strip_prefix("image:") {
        Some(reference) => (reference, true),
        None => (raw, false),
    };
    check_oci_reference(reference)?;
    Ok(FlyImage::Reference {
        reference: reference.to_owned(),
        prebaked,
    })
}

/// The distribution-spec reference grammar:
/// `[domain[:port]/]component(/component)*[:tag][@algorithm:hex]`.
fn check_oci_reference(raw: &str) -> Result<(), ProviderAdmissionError> {
    const REASON: &str = "image reference is malformed";
    if raw.is_empty() {
        return Err(err("image", REASON));
    }
    if raw.len() > MAX_PROVIDER_IMAGE_BYTES {
        return Err(err("image", "value exceeds its bounded length"));
    }
    let (name_and_tag, digest) = match raw.split_once('@') {
        Some((name, digest)) => (name, Some(digest)),
        None => (raw, None),
    };
    if let Some(digest) = digest {
        let valid = match digest.split_once(':') {
            Some(("sha256", hex)) => hex.len() == 64 && lower_hex(hex),
            Some(("sha512", hex)) => hex.len() == 128 && lower_hex(hex),
            _ => false,
        };
        if !valid {
            return Err(err("image", REASON));
        }
    }
    let last_slash = name_and_tag.rfind('/');
    let (name, tag) = match name_and_tag.rfind(':') {
        Some(colon) if last_slash.is_none_or(|slash| colon > slash) => {
            (&name_and_tag[..colon], Some(&name_and_tag[colon + 1..]))
        }
        _ => (name_and_tag, None),
    };
    if let Some(tag) = tag {
        let bytes = tag.as_bytes();
        let valid = !bytes.is_empty()
            && bytes.len() <= MAX_IMAGE_TAG_BYTES
            && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
        if !valid {
            return Err(err("image", REASON));
        }
    }
    if name.is_empty() || name.len() > MAX_IMAGE_NAME_BYTES {
        return Err(err("image", REASON));
    }
    let mut components = name.split('/');
    let first = components.next().unwrap_or_default();
    let has_more = name.contains('/');
    let first_is_domain =
        has_more && (first.contains('.') || first.contains(':') || first == "localhost");
    if first_is_domain {
        let (host, port) = match first.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (first, None),
        };
        let port_ok = port.is_none_or(|port| {
            !port.is_empty()
                && port.len() <= 5
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port != 0)
        });
        if !valid_host(host) || !port_ok {
            return Err(err("image", REASON));
        }
    } else if !oci_path_component(first) {
        return Err(err("image", REASON));
    }
    if !components.all(oci_path_component) {
        return Err(err("image", REASON));
    }
    Ok(())
}

fn lower_hex(raw: &str) -> bool {
    raw.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`
fn oci_path_component(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if alnum(byte) {
            index += 1;
            continue;
        }
        // A separator run: exactly `.`, `_`, `__`, or one-or-more `-`.
        let start = index;
        while index < bytes.len() && !alnum(bytes[index]) {
            index += 1;
        }
        let separator = &raw[start..index];
        let valid =
            matches!(separator, "." | "_" | "__") || separator.bytes().all(|byte| byte == b'-');
        if !valid {
            return false;
        }
    }
    true
}

/// VPS images are vendor image slugs/ids, or `snapshot:<positive integer>`.
/// Any other prefixed form (including `image:`) is unsupported by the VPS
/// request shapers and is refused rather than sent as an image name.
fn vps_image(raw: &str) -> Result<VpsImage, ProviderAdmissionError> {
    if raw.is_empty() {
        return Ok(VpsImage::Default);
    }
    if let Some(id) = raw.strip_prefix("snapshot:") {
        if id.is_empty()
            || id.len() > MAX_SNAPSHOT_DIGITS
            || !id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(err("image", "snapshot id is malformed"));
        }
        let Ok(value) = id.parse::<i64>() else {
            return Err(err("image", "snapshot id is out of range"));
        };
        if value <= 0 {
            return Err(err("image", "snapshot id must be positive"));
        }
        // Canonical decimal: `snapshot:007` names the same id as `snapshot:7`.
        return Ok(VpsImage::Snapshot(value.to_string()));
    }
    if raw.contains(':') {
        return Err(err(
            "image",
            "image form is not supported by the VPS adapter",
        ));
    }
    let bytes = raw.as_bytes();
    let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let valid = raw.len() <= MAX_VPS_IMAGE_BYTES
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|byte| edge(*byte) || matches!(byte, b'-' | b'.' | b'_'));
    if !valid {
        return Err(err("image", "image slug is malformed"));
    }
    Ok(VpsImage::Image(raw.to_owned()))
}

fn instance_cap(value: u32) -> Result<u32, ProviderAdmissionError> {
    if value > MAX_PROVIDER_INSTANCES {
        Err(err(
            "max_instances",
            "value exceeds the provider safety bound",
        ))
    } else {
        Ok(value)
    }
}

fn lifetime(value: u64) -> Result<u64, ProviderAdmissionError> {
    if value > crate::time_policy::MAX_DURATION_SECS {
        Err(err(
            "max_lifetime_secs",
            "value exceeds the duration safety bound",
        ))
    } else {
        Ok(value)
    }
}

/// Validate the iroh call-home values before any request body is built. The
/// sandbox id must name the sandbox being created, so a mismatched injection
/// cannot authorize a different machine.
fn check_iroh(
    iroh: &IrohInjectionInput<'_>,
    name: Option<&str>,
) -> Result<(), ProviderAdmissionError> {
    let home = iroh.home_node;
    if home.is_empty()
        || home.len() > MAX_PROVIDER_INJECTION_BYTES
        || !home
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(err("iroh.home_node", "endpoint id is malformed"));
    }
    let auth = iroh.sandbox_auth;
    let token_ok = auth.len() <= MAX_PROVIDER_INJECTION_BYTES
        && auth
            .strip_prefix("tgi_")
            .is_some_and(|hex| hex.len() >= 32 && hex.len() % 2 == 0 && lower_hex(hex));
    if !token_ok {
        return Err(err("iroh.sandbox_auth", "sandbox token is malformed"));
    }
    if name != Some(iroh.sandbox_id) {
        return Err(err(
            "iroh.sandbox_id",
            "sandbox id does not match the sandbox name",
        ));
    }
    Ok(())
}

/// Parse an OpenSSH public-key line structurally: `<type> <base64> [comment]`
/// where the decoded blob is the SSH wire encoding of a key of the same type
/// with the algorithm's exact component sizes.
fn check_pubkey(raw: &str) -> Result<(), ProviderAdmissionError> {
    const REASON: &str = "public key shape is invalid";
    if raw.is_empty() || raw.len() > MAX_PROVIDER_KEY_BYTES || raw.chars().any(char::is_control) {
        return Err(err("pubkey", REASON));
    }
    let raw = raw.trim();
    let (kind, rest) = raw.split_once(' ').ok_or(err("pubkey", REASON))?;
    let blob = rest.split(' ').next().unwrap_or_default();
    let decoded = decode_base64(blob).ok_or(err("pubkey", REASON))?;
    let mut wire = Wire(&decoded);
    if wire.string().ok_or(err("pubkey", REASON))? != kind.as_bytes() {
        return Err(err("pubkey", REASON));
    }
    let valid = match kind {
        "ssh-ed25519" => wire.string().is_some_and(|key| key.len() == 32),
        "sk-ssh-ed25519@openssh.com" => {
            wire.string().is_some_and(|key| key.len() == 32)
                && wire.string().is_some_and(|app| !app.is_empty())
        }
        "ecdsa-sha2-nistp256" => ecdsa(&mut wire, b"nistp256", 65),
        "ecdsa-sha2-nistp384" => ecdsa(&mut wire, b"nistp384", 97),
        "ecdsa-sha2-nistp521" => ecdsa(&mut wire, b"nistp521", 133),
        "sk-ecdsa-sha2-nistp256@openssh.com" => {
            ecdsa(&mut wire, b"nistp256", 65) && wire.string().is_some_and(|app| !app.is_empty())
        }
        "ssh-rsa" => rsa(&mut wire),
        _ => false,
    };
    if !valid || !wire.0.is_empty() {
        return Err(err("pubkey", REASON));
    }
    Ok(())
}

fn ecdsa(wire: &mut Wire<'_>, curve: &[u8], point_len: usize) -> bool {
    wire.string() == Some(curve)
        && wire
            .string()
            .is_some_and(|point| point.len() == point_len && point[0] == 0x04)
}

fn rsa(wire: &mut Wire<'_>) -> bool {
    let Some(exponent) = wire.mpint() else {
        return false;
    };
    let Some(modulus) = wire.mpint() else {
        return false;
    };
    let exponent_ok = !exponent.is_empty()
        && exponent.len() <= 8
        && exponent[exponent.len() - 1] & 1 == 1
        && !(exponent.len() == 1 && exponent[0] < 3);
    exponent_ok
        && (MIN_RSA_MODULUS_BYTES..=MAX_RSA_MODULUS_BYTES).contains(&modulus.len())
        && modulus[modulus.len() - 1] & 1 == 1
}

struct Wire<'a>(&'a [u8]);

impl<'a> Wire<'a> {
    fn string(&mut self) -> Option<&'a [u8]> {
        let (len, rest) = self.0.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_be_bytes(*len)).ok()?;
        if len > rest.len() {
            return None;
        }
        let (value, rest) = rest.split_at(len);
        self.0 = rest;
        Some(value)
    }

    /// A positive SSH mpint with its sign-padding byte stripped. Negative or
    /// non-minimal encodings are refused.
    fn mpint(&mut self) -> Option<&'a [u8]> {
        let value = self.string()?;
        let first = *value.first()?;
        if first & 0x80 != 0 {
            return None;
        }
        if first == 0 {
            let rest = &value[1..];
            // A leading zero is only legal when the next byte's top bit is set.
            return rest.first().filter(|byte| **byte & 0x80 != 0).map(|_| rest);
        }
        Some(value)
    }
}

/// Strict, bounded standard-alphabet base64 with mandatory canonical padding.
fn decode_base64(raw: &str) -> Option<Vec<u8>> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 != 0 || bytes.len() > MAX_PROVIDER_KEY_BYTES {
        return None;
    }
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunks = bytes.len() / 4;
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = index + 1 == chunks;
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut acc = 0u32;
        for (position, byte) in chunk.iter().enumerate() {
            let digit = if position >= 4 - padding {
                0
            } else {
                u32::from(value(*byte)?)
            };
            acc = (acc << 6) | digit;
        }
        let decoded = acc.to_be_bytes();
        let produced = 3 - padding;
        // Canonical padding: the discarded low bits must be zero.
        if (padding == 1 && decoded[3] != 0) || (padding == 2 && decoded[2..] != [0, 0]) {
            return None;
        }
        out.extend_from_slice(&decoded[1..=produced]);
    }
    Some(out)
}

#[cfg(test)]
#[path = "provider_admission_tests.rs"]
mod tests;
