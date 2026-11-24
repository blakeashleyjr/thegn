//! Versioned wire types and the bounded environment transport for the Windows
//! sandbox helper.
//!
//! Everything here is plain data plus JSON encoding: no OS calls, no process
//! handles, no logging. The helper makes the policy decisions (it refuses an
//! unknown [`WIN_PROTO_VERSION`] with exit 3 and a `.bat`/`.cmd` `argv[0]` with
//! exit 5); this crate only fixes the shapes both sides decode.
//!
//! Error values carry fixed strings and chunk indices only. A request carries
//! the explicit workload environment and, later, a proxy URL with a lease token,
//! so nothing in an error may echo request content (B30).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Wire protocol version. The helper refuses any other value with exit 3.
pub const WIN_PROTO_VERSION: u32 = 1;

/// Environment-variable prefix for the numbered request chunks.
pub const REQ_ENV_PREFIX: &str = "THEGN_WINSB_REQ_";

/// Maximum size of one request chunk, in UTF-8 bytes.
pub const REQ_CHUNK_BYTES: usize = 8 * 1024;

/// Maximum size of the whole serialized request.
pub const REQ_MAX_BYTES: usize = 1 << 20;

/// A launch request, lowered by core and decoded by the helper.
///
/// `deny_unknown_fields` makes version skew fail closed: a field the helper
/// does not know is an error rather than something silently dropped.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WinSandboxRequest {
    /// Must equal [`WIN_PROTO_VERSION`]; the helper refuses anything else.
    pub version: u32,
    pub mode: WinMode,
    /// `argv[0]` is an absolute `.exe` resolved by thegn; never a `.bat`/`.cmd`.
    pub argv: Vec<String>,
    /// Absolute Windows path.
    pub cwd: String,
    /// Explicit workload environment; Unicode only.
    pub env: Vec<(String, String)>,
    pub job: WinJobLimits,
    /// `Some` iff `mode == WinMode::Mxc`.
    pub mxc: Option<WinMxcPolicy>,
    /// `Console` for panes; `Pipes` only for the egress service launch.
    pub stdio: WinStdio,
    /// PSEC container/profile name; `Some` only for the egress service launch.
    pub profile_name: Option<String>,
    pub status_path: Option<String>,
}

/// Which containment mechanism the helper applies.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WinMode {
    Job,
    Mxc,
}

/// How the workload's standard handles are provided.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WinStdio {
    Console,
    Pipes,
}

/// Job Object limits. `None` means the limit is not set.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct WinJobLimits {
    /// `JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP` in 1/100 percent of ALL logical
    /// CPUs, in `1..=10000`.
    pub cpu_rate: Option<u16>,
    /// `JOB_OBJECT_LIMIT_JOB_MEMORY`: memory committed by the job, which is not
    /// the job's resident set size.
    pub job_commit_bytes: Option<u64>,
    /// `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`: a process count, not a thread count.
    pub active_processes: Option<u32>,
    pub ui: WinUiRestrictions,
}

/// `JOBOBJECT_BASIC_UI_RESTRICTIONS` bits, one field per `JOB_OBJECT_UILIMIT_*`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct WinUiRestrictions {
    pub read_clipboard: bool,
    pub write_clipboard: bool,
    pub desktop: bool,
    pub display_settings: bool,
    pub exit_windows: bool,
    pub global_atoms: bool,
    pub system_parameters: bool,
    pub handles: bool,
}

/// AppContainer/PSEC policy for a `WinMode::Mxc` launch.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WinMxcPolicy {
    pub fs: WinFsPolicy,
    pub net: WinNetPolicy,
    /// The helper may only apply `fs.deny` if the PSEC schema supports it.
    pub require_fs_deny: bool,
}

/// PSEC filesystem policy. Paths are absolute Windows paths.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct WinFsPolicy {
    pub read_write: Vec<String>,
    pub read_only: Vec<String>,
    pub deny: Vec<String>,
    /// PSEC `fs_enumerate` paths (schema 1.1).
    pub enumerate: Vec<String>,
}

/// PSEC network policy, tagged by `kind` so an unknown policy fails closed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WinNetPolicy {
    /// Egress deny, ingress deny, host loopback deny.
    None,
    /// `network = "host"`: all network traffic allowed.
    Open,
    /// `network = "nat"`: egress allowed minus the CIDR denies.
    Outbound {
        deny: Vec<String>,
        host_loopback: bool,
    },
    /// Egress through the local proxy only. With a `peer` the policy is
    /// identity-scoped and host loopback is denied; without one it is
    /// identity-less and host loopback is allowed.
    ProxyOnly {
        proxy_url: String,
        peer: Option<String>,
    },
}

/// What the helper's capability probe found on this machine.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WinProbeReport {
    pub version: u32,
    pub os: WinOsVersion,
    /// Whether Job Objects are available at all.
    pub job_objects: bool,
    /// Whether CPU rate control also applies inside a nested job. Filled by
    /// WS06-P1-06; `None` means unknown.
    pub cpu_rate_nested: Option<bool>,
    /// Fixed, non-remote drive roots the workload may map.
    pub local_volumes: Vec<String>,
    pub psec: PsecSupport,
}

/// The running Windows version, including the UBR (patch) revision.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WinOsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    pub ubr: u32,
    pub server: bool,
    pub edition: String,
}

/// Which PSEC behaviours the probe proved on this machine.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PsecSupport {
    pub exports: bool,
    pub schema_1_0: bool,
    pub schema_1_1: bool,
    pub create_close_ok: bool,
    pub fs_deny: bool,
    pub identityless_loopback_proxy: bool,
    /// A request-aware probe accepted an identity-scoped proxy policy.
    pub proxy_peer: bool,
    /// Fixed-shape diagnostic string; never request content.
    pub error: Option<String>,
}

/// What the helper applied and what it had to degrade, written to `status_path`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HelperStatus {
    pub version: u32,
    pub mode: WinMode,
    pub child_pid: u32,
    /// What `SetInformationJobObject` accepted, which may be less than requested.
    pub applied: WinJobLimits,
    /// Degradations such as `ui-limits-rejected-on-nested-job` or
    /// `job-wait-timeout`.
    pub degraded: Vec<String>,
    /// `Some` only for the egress service launch.
    pub peer: Option<PeerIdentity>,
}

/// The AppContainer identity and user SID a proxied launch ran as.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PeerIdentity {
    pub profile: String,
    pub sid: String,
}

/// A request-transport failure. Renderings are fixed strings or indices only, so
/// a failure never prints request content (B30).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtoError {
    /// The serialized request, or the reassembled chunks, exceeds [`REQ_MAX_BYTES`].
    RequestTooLarge,
    /// No `THEGN_WINSB_REQ_*` entry was present.
    MissingRequest,
    /// A request-prefixed entry's suffix is not a decimal chunk index.
    InvalidChunkKey,
    /// Two entries named the same chunk index.
    DuplicateChunk(usize),
    /// The chunk indices are not contiguous from zero.
    MissingChunk(usize),
    /// One chunk exceeds [`REQ_CHUNK_BYTES`].
    ChunkTooLarge(usize),
    /// The reassembled chunks are not a well-formed [`WinSandboxRequest`].
    InvalidJson,
    /// The request declared a protocol version this build does not speak.
    UnsupportedVersion(u32),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestTooLarge => {
                write!(f, "sandbox request exceeds {REQ_MAX_BYTES} bytes")
            }
            Self::MissingRequest => write!(f, "sandbox request environment is missing"),
            Self::InvalidChunkKey => {
                write!(f, "sandbox request chunk name is not a decimal index")
            }
            Self::DuplicateChunk(index) => {
                write!(f, "sandbox request chunk {index} is duplicated")
            }
            Self::MissingChunk(index) => write!(f, "sandbox request chunk {index} is missing"),
            Self::ChunkTooLarge(index) => {
                write!(
                    f,
                    "sandbox request chunk {index} exceeds {REQ_CHUNK_BYTES} bytes"
                )
            }
            Self::InvalidJson => write!(f, "sandbox request JSON is invalid"),
            Self::UnsupportedVersion(version) => {
                write!(f, "sandbox protocol version {version} is unsupported")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

/// Serialize `req` into numbered environment entries the helper can receive.
///
/// Chunks end only on Unicode scalar boundaries, so Windows converting each
/// value to UTF-16 never meets a split character or half a surrogate pair.
pub fn encode_request_env(req: &WinSandboxRequest) -> Result<Vec<(String, String)>, ProtoError> {
    let json = serde_json::to_string(req).map_err(|_| ProtoError::InvalidJson)?;
    if json.len() > REQ_MAX_BYTES {
        return Err(ProtoError::RequestTooLarge);
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < json.len() {
        let mut end = (start + REQ_CHUNK_BYTES).min(json.len());
        while !json.is_char_boundary(end) {
            end -= 1;
        }
        let index = chunks.len();
        chunks.push((
            format!("{REQ_ENV_PREFIX}{index}"),
            json[start..end].to_owned(),
        ));
        start = end;
    }
    Ok(chunks)
}

/// Remove every request-prefixed entry from `env` and decode the request.
///
/// The entries are removed before any validation runs, including on malformed
/// transports, so the caller can hand the remaining `env` to the workload
/// without leaking the request or the proxy URL into it.
///
/// Windows environment names are case-insensitive, so the scrub is too.
pub fn decode_request_env(
    env: &mut Vec<(String, String)>,
) -> Result<WinSandboxRequest, ProtoError> {
    let mut chunks = Vec::new();
    env.retain(|(name, value)| match chunk_index(name) {
        Some(suffix) => {
            chunks.push((suffix.to_owned(), value.clone()));
            false
        }
        None => true,
    });

    if chunks.is_empty() {
        return Err(ProtoError::MissingRequest);
    }
    if chunks.len() > REQ_MAX_BYTES.div_ceil(REQ_CHUNK_BYTES) {
        return Err(ProtoError::RequestTooLarge);
    }

    let mut numbered = Vec::with_capacity(chunks.len());
    for (suffix, value) in chunks {
        let index = suffix
            .parse::<usize>()
            .map_err(|_| ProtoError::InvalidChunkKey)?;
        if value.len() > REQ_CHUNK_BYTES {
            return Err(ProtoError::ChunkTooLarge(index));
        }
        numbered.push((index, value));
    }
    numbered.sort_unstable_by_key(|(index, _)| *index);
    if let Some(pair) = numbered.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(ProtoError::DuplicateChunk(pair[0].0));
    }

    let mut json = String::new();
    for (expected, (index, value)) in numbered.into_iter().enumerate() {
        if index != expected {
            return Err(ProtoError::MissingChunk(expected));
        }
        if json.len() + value.len() > REQ_MAX_BYTES {
            return Err(ProtoError::RequestTooLarge);
        }
        json.push_str(&value);
    }

    let request: WinSandboxRequest =
        serde_json::from_str(&json).map_err(|_| ProtoError::InvalidJson)?;
    if request.version != WIN_PROTO_VERSION {
        return Err(ProtoError::UnsupportedVersion(request.version));
    }
    Ok(request)
}

/// The chunk-index suffix of a request-named environment entry, if `name` names
/// one. Names are matched case-insensitively, as the Windows environment is.
fn chunk_index(name: &str) -> Option<&str> {
    let prefix = name.get(..REQ_ENV_PREFIX.len())?;
    if prefix.eq_ignore_ascii_case(REQ_ENV_PREFIX) {
        name.get(REQ_ENV_PREFIX.len()..)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> WinSandboxRequest {
        WinSandboxRequest {
            version: WIN_PROTO_VERSION,
            mode: WinMode::Mxc,
            argv: vec![r"C:\work\app.exe".to_owned(), "a value".to_owned()],
            cwd: r"C:\work".to_owned(),
            env: vec![("UNICODE".to_owned(), "snowman ☃".to_owned())],
            job: WinJobLimits::default(),
            mxc: Some(WinMxcPolicy {
                fs: WinFsPolicy {
                    read_write: vec![r"C:\work".to_owned()],
                    ..WinFsPolicy::default()
                },
                net: WinNetPolicy::ProxyOnly {
                    proxy_url: "http://127.0.0.1:1234".to_owned(),
                    peer: Some("profile-a".to_owned()),
                },
                require_fs_deny: true,
            }),
            stdio: WinStdio::Console,
            profile_name: None,
            status_path: Some(r"C:\work\status.json".to_owned()),
        }
    }

    /// Chunk a raw JSON document the way [`encode_request_env`] chunks a request,
    /// so a test can build a transport no encoder would produce.
    fn chunk_json(json: &str) -> Vec<(String, String)> {
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < json.len() {
            let mut end = (start + REQ_CHUNK_BYTES).min(json.len());
            while !json.is_char_boundary(end) {
                end -= 1;
            }
            let index = chunks.len();
            chunks.push((
                format!("{REQ_ENV_PREFIX}{index}"),
                json[start..end].to_owned(),
            ));
            start = end;
        }
        chunks
    }

    #[test]
    fn request_env_roundtrip_large() {
        let mut request = fixture();
        request.env = vec![("BIG".to_owned(), "x".repeat(900 * 1024))];
        let chunks = encode_request_env(&request).unwrap();
        assert!(chunks.len() > 100, "expected a multi-chunk transport");
        assert!(
            chunks
                .iter()
                .all(|(_, chunk)| chunk.len() <= REQ_CHUNK_BYTES)
        );
        assert_eq!(chunks[0].0, format!("{REQ_ENV_PREFIX}0"));

        let mut env = vec![("KEEP".to_owned(), "value".to_owned())];
        env.extend(chunks);
        assert_eq!(decode_request_env(&mut env).unwrap(), request);
        assert_eq!(env, [("KEEP".to_owned(), "value".to_owned())]);
    }

    #[test]
    fn request_env_chunks_stay_on_unicode_boundaries() {
        let mut request = fixture();
        request.env = vec![("SNOW".to_owned(), "☃".repeat(REQ_CHUNK_BYTES))];
        let chunks = encode_request_env(&request).unwrap();
        assert!(chunks.len() > 1);
        let mut env = chunks;
        assert_eq!(decode_request_env(&mut env).unwrap(), request);
    }

    #[test]
    fn request_env_rejects_missing_chunk() {
        let mut request = fixture();
        request.env = vec![("BIG".to_owned(), "x".repeat(REQ_CHUNK_BYTES * 4))];
        let mut env = encode_request_env(&request).unwrap();
        assert!(env.len() > 3);
        env.remove(3);
        assert_eq!(
            decode_request_env(&mut env),
            Err(ProtoError::MissingChunk(3))
        );
        assert!(
            env.is_empty(),
            "a failed decode must still scrub the transport"
        );
    }

    #[test]
    fn request_env_rejects_duplicate_chunk() {
        let mut request = fixture();
        request.env = vec![("BIG".to_owned(), "x".repeat(REQ_CHUNK_BYTES * 2))];
        let mut env = encode_request_env(&request).unwrap();
        env[1].0 = format!("{REQ_ENV_PREFIX}0");
        assert_eq!(
            decode_request_env(&mut env),
            Err(ProtoError::DuplicateChunk(0))
        );
    }

    #[test]
    fn request_env_scrubs() {
        let mut request = fixture();
        request.env = vec![("BIG".to_owned(), "x".repeat(REQ_CHUNK_BYTES * 3))];
        let mut env = vec![("KEEP".to_owned(), "value".to_owned())];
        env.extend(encode_request_env(&request).unwrap());
        // Windows environment names are case-insensitive; so is the scrub.
        env[1].0.make_ascii_lowercase();
        assert_eq!(decode_request_env(&mut env).unwrap(), request);
        assert_eq!(env, [("KEEP".to_owned(), "value".to_owned())]);
    }

    #[test]
    fn request_env_scrubs_private_values_before_failing() {
        let mut env = vec![
            ("KEEP".to_owned(), "value".to_owned()),
            (format!("{REQ_ENV_PREFIX}0"), "not json".to_owned()),
            (format!("{REQ_ENV_PREFIX}bogus"), "secret".to_owned()),
        ];
        assert_eq!(
            decode_request_env(&mut env),
            Err(ProtoError::InvalidChunkKey)
        );
        assert_eq!(env, [("KEEP".to_owned(), "value".to_owned())]);
        assert!(!ProtoError::InvalidChunkKey.to_string().contains("secret"));
    }

    #[test]
    fn request_env_rejects_an_oversized_request() {
        let mut request = fixture();
        request.env = vec![("BIG".to_owned(), "x".repeat(REQ_MAX_BYTES))];
        assert_eq!(
            encode_request_env(&request),
            Err(ProtoError::RequestTooLarge)
        );
    }

    #[test]
    fn version_field_required() {
        let mut value = serde_json::to_value(fixture()).unwrap();
        assert!(value.as_object_mut().unwrap().remove("version").is_some());
        let json = serde_json::to_string(&value).unwrap();
        let mut env = chunk_json(&json);
        assert_eq!(decode_request_env(&mut env), Err(ProtoError::InvalidJson));
        assert!(env.is_empty());
    }

    #[test]
    fn unsupported_version_is_rejected_without_echoing_the_request() {
        let mut request = fixture();
        request.version = WIN_PROTO_VERSION + 1;
        let mut env = encode_request_env(&request).unwrap();
        let error = decode_request_env(&mut env).unwrap_err();
        assert_eq!(error, ProtoError::UnsupportedVersion(WIN_PROTO_VERSION + 1));
        assert!(!error.to_string().contains("profile-a"));
        assert!(env.is_empty());
    }

    #[test]
    fn request_rejects_unknown_fields() {
        let mut value = serde_json::to_value(fixture()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_owned(), serde_json::Value::Bool(true));
        let json = serde_json::to_string(&value).unwrap();
        let mut env = chunk_json(&json);
        assert_eq!(decode_request_env(&mut env), Err(ProtoError::InvalidJson));
        assert!(env.is_empty());
    }

    #[test]
    fn wire_names_are_lowercase_for_modes_and_stdio() {
        assert_eq!(serde_json::to_string(&WinMode::Mxc).unwrap(), "\"mxc\"");
        assert_eq!(serde_json::to_string(&WinMode::Job).unwrap(), "\"job\"");
        assert_eq!(
            serde_json::to_string(&WinStdio::Pipes).unwrap(),
            "\"pipes\""
        );
        assert_eq!(
            serde_json::to_string(&WinStdio::Console).unwrap(),
            "\"console\""
        );
    }

    #[test]
    fn winnet_policy_serde_shapes() {
        let cases = [
            (WinNetPolicy::None, serde_json::json!({ "kind": "none" })),
            (WinNetPolicy::Open, serde_json::json!({ "kind": "open" })),
            (
                WinNetPolicy::Outbound {
                    deny: vec!["10.0.0.0/8".to_owned()],
                    host_loopback: false,
                },
                serde_json::json!({
                    "kind": "outbound",
                    "deny": ["10.0.0.0/8"],
                    "host_loopback": false,
                }),
            ),
            (
                WinNetPolicy::ProxyOnly {
                    proxy_url: "http://127.0.0.1:9".to_owned(),
                    peer: Some("profile-a".to_owned()),
                },
                serde_json::json!({
                    "kind": "proxy-only",
                    "proxy_url": "http://127.0.0.1:9",
                    "peer": "profile-a",
                }),
            ),
        ];
        for (policy, shape) in cases {
            assert_eq!(serde_json::to_value(&policy).unwrap(), shape);
            assert_eq!(
                serde_json::from_value::<WinNetPolicy>(shape).unwrap(),
                policy
            );
        }
    }

    #[test]
    fn status_and_probe_round_trip() {
        let status = HelperStatus {
            version: WIN_PROTO_VERSION,
            mode: WinMode::Job,
            child_pid: 4242,
            applied: WinJobLimits {
                cpu_rate: Some(5_000),
                job_commit_bytes: Some(1 << 30),
                active_processes: Some(8),
                ui: WinUiRestrictions {
                    read_clipboard: false,
                    ..WinUiRestrictions::default()
                },
            },
            degraded: vec!["job-wait-timeout".to_owned()],
            peer: Some(PeerIdentity {
                profile: "profile-a".to_owned(),
                sid: "S-1-15-2-1".to_owned(),
            }),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<HelperStatus>(&json).unwrap(), status);

        let probe = WinProbeReport {
            version: WIN_PROTO_VERSION,
            os: WinOsVersion {
                major: 10,
                minor: 0,
                build: 26_100,
                ubr: 1,
                server: false,
                edition: "Windows 11 Pro".to_owned(),
            },
            job_objects: true,
            cpu_rate_nested: None,
            local_volumes: vec![r"C:\".to_owned()],
            psec: PsecSupport {
                exports: true,
                schema_1_0: true,
                schema_1_1: true,
                create_close_ok: true,
                fs_deny: true,
                identityless_loopback_proxy: true,
                proxy_peer: true,
                error: None,
            },
        };
        let json = serde_json::to_string(&probe).unwrap();
        assert_eq!(
            serde_json::from_str::<WinProbeReport>(&json).unwrap(),
            probe
        );
    }
}
