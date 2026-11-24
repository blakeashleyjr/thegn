//! The egress control wire, the lease spec and the audit record v1: **frozen
//! data shapes only** (WS04-P1-07).
//!
//! # Why this module exists before the proxy
//!
//! The proxy (`thegn-egress`), the daemon client, the audit sink and the
//! approval flow are separate items. Freezing the shapes first lets them be
//! written against one contract: a field renamed here later is a
//! [`PROTO_VERSION`] bump and a new socket directory, not a silent mismatch.
//! Nothing in this module does I/O: the framing helper operates on a
//! [`BufRead`] and every other item is plain data.
//!
//! # The wire (NDJSON)
//!
//! One request or response per line, UTF-8, at most [`MAX_LINE`] bytes
//! (excluding the newline and an optional trailing `\r`). A longer line is an
//! error: the connection is closed,
//! never buffered.
//!
//! Requests carry an id: `{"id":3,"op":"list"}`. Responses echo it on success
//! (`{"id":3,"ok":...}`) or failure (`{"id":3,"err":{"code":..,"msg":..}}`).
//! Events are server pushes with no id: `{"event":"dropped","n":12}`.
//!
//! ```text
//! <- {"event":"challenge","proto":1,"nonce":"9c1e…"}
//! -> {"id":1,"op":"hello","proto":1,"client":"thegn/0.1.0","mac":"5f2d…"}
//! <- {"id":1,"ok":{"proto":1,"git_sha":"eefb04b29","features":["approvals"]}}
//! -> {"id":2,"op":"lease", … }
//! <- {"id":2,"ok":{"id":"0707…","lease_user":"lease-0707","token":"…","endpoint":{…}}}
//! <- {"event":"decision","v":1,"ts":"…","lease":"0707…", … }
//! ```
//!
//! **Handshake.** The server speaks first: on accept it sends one
//! [`ControlEvent::Challenge`] carrying a fresh random `nonce` (lowercase hex,
//! at least 16 bytes of entropy) and its own [`PROTO_VERSION`]. The client's
//! `hello` answers with `mac = lowercase-hex(HMAC-SHA256(secret, nonce ‖
//! client))`, where `nonce` is the challenge's hex text as received (not
//! decoded) and `client` is the `hello`'s `client` string, both as UTF-8 bytes.
//! A nonce is single use: it is bound to its connection and never reissued.
//!
//! **The lease reply** is a [`LeaseGrantWire`] as the `ok` payload. It is the
//! only message that carries the bearer token; only the authenticated socket
//! carries it, and persistence stores the token's SHA-256, never the token.
//!
//! # Freeze rules
//!
//! 1. **Additive only within v1.** Unknown fields are ignored on read, so v1.1
//!    may add a field; a field may not be renamed, removed or change meaning.
//! 2. **Strings, with an `unknown` escape,** for the vocabularies a reader
//!    consumes: `proto`, `decision`, `reason`, `why` and the `event` tag. Each is
//!    a closed enum for the writer, but carries an `Unknown` arm (spelled
//!    `unknown`) that any unrecognised value decodes to, so a v1 reader survives
//!    a value a later v1.x writer adds. Request-side vocabularies (`op`,
//!    `phase`, `approvals`, `verdict`) stay strict: a server must refuse a
//!    request it does not understand.
//! 3. **Secrets never enter a record.** [`LeaseToken`] and the value inside a
//!    [`ResolvedCredential`] never appear in `Debug` (they are the `plan`
//!    module's redacted types), a [`ResolvedCredential`] never serialises its
//!    value, and the audit record has no field for them (B13).
//! 4. **The audit record (`v: 1`) is a published format.** Terminal width,
//!    locale and theme never change a field's bytes.
//!
//! # Ownership
//!
//! This item owns the shapes. The evaluator is WS04-P1-03, presets and
//! `resolve_egress` are WS04-P1-05, the proxy and its persistence are
//! WS04-P2-01a/P2-01b. [`EgressProxyLimits`] lives in
//! [`crate::egress_nft`] and is reused verbatim; its serde
//! impls are here so that module is untouched.

use std::io::{self, BufRead};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The control-wire version.
///
/// Carried by [`ControlRequest::Hello`] and baked into the socket and lease
/// paths (`v1`). An incompatible change is a bump and a new path; v1 is never
/// reinterpreted in place.
pub const PROTO_VERSION: u32 = 1;

/// The NDJSON line cap in bytes, excluding the newline. See
/// [`read_line_bounded`].
pub const MAX_LINE: usize = 64 * 1024;

/// The audit record schema version written into [`AuditRecord::v`].
///
/// v1 is the published format described by [`AuditRecord`]. Additive fields keep
/// `1`; a removed field or a changed meaning bumps it.
pub const AUDIT_SCHEMA_VERSION: u8 = 1;

/// The longest host recorded in an audit record, in bytes. Longer hosts are
/// truncated by [`AuditRecord::bounded`].
pub const MAX_AUDIT_HOST_LEN: usize = 253;

/// The longest SNI recorded in an audit record, in bytes.
pub const MAX_AUDIT_SNI_LEN: usize = 253;

/// The longest HTTP method recorded in an audit record, in bytes.
pub const MAX_AUDIT_METHOD_LEN: usize = 16;

/// The longest request target recorded in an audit record, in bytes. Only the
/// path is recorded, never the query.
pub const MAX_AUDIT_PATH_LEN: usize = 512;

/// The longest `reason` carried by [`ControlEvent::ApprovalRequested`], in
/// bytes. The host uses [`MAX_AUDIT_HOST_LEN`].
pub const MAX_APPROVAL_REASON_LEN: usize = 512;

/// A lease's approval policy (the plan's `ApprovalMode`): the same vocabulary as
/// the policy's `approvals` key. The lease carries it once, as
/// [`EgressPolicy::approvals`]; [`LeaseSpec`] has no second copy.
pub use crate::egress::policy::EgressApprovals as ApprovalMode;

/// A lease's audit level (the plan's `AuditLevel`): the same vocabulary as the
/// policy's `audit` key.
pub use crate::egress::policy::EgressAuditLevel as AuditLevel;

/// The proxy's resource limits, reused verbatim from the nftables seed and
/// re-exported here so consumers of the wire import one module.
pub use crate::egress_nft::EgressProxyLimits;

use crate::egress::plan::{LeaseId, LeaseToken, ProxyEndpoint};
use crate::egress::policy::EgressPolicy;

/// What a redacted field prints instead of its secret.
const REDACTED: &str = "***";

/// Read one NDJSON line from `reader`, capped at `max` bytes excluding the
/// newline.
///
/// A line longer than `max` is an [`io::ErrorKind::InvalidData`] error and the
/// reader is left immediately after the bytes examined so far: a runaway line
/// never grows a buffer. A line that is not UTF-8 is also `InvalidData`; the
/// trailing `\n` and an optional `\r` are stripped, and end of stream with
/// nothing left is [`io::ErrorKind::UnexpectedEof`].
///
/// The cap includes neither `\n` nor a trailing `\r`, so a caller may pass
/// [`MAX_LINE`] and a CRLF line of exactly that many content bytes is accepted.
pub fn read_line_bounded<R: BufRead>(reader: &mut R, max: usize) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            if line.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "egress wire: no line before end of stream",
                ));
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let (chunk, used) = match newline {
            Some(index) => (&available[..index], index + 1),
            None => (available, available.len()),
        };
        // One extra byte is tolerated while reading, for a trailing `\r`; the
        // exact cap is enforced after it is stripped.
        if line.len() + chunk.len() > max.saturating_add(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("egress wire: line exceeds {max} bytes"),
            ));
        }
        line.extend_from_slice(chunk);
        reader.consume(used);
        if newline.is_some() {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("egress wire: line exceeds {max} bytes"),
        ));
    }
    String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "egress wire: line is not UTF-8"))
}

/// One request line: the client's id plus the operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlRequestLine {
    /// The client's request id. Responses echo it; events never carry it.
    pub id: u64,
    /// The operation, flattened into the same object under `op`.
    #[serde(flatten)]
    pub request: ControlRequest,
}

/// A control request (WS04 §3.2). Every variant but [`Self::Hello`] needs an
/// authenticated, leased client; the proxy's authorisation is WS04-P2-01b's
/// business, not this module's.
// A lease request carries the whole resolved policy, so its variant is much
// larger than `Hello`; boxing it would add an allocation and change nothing on
// the wire.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// The first request on the socket: prove the client knows the shared
    /// secret before any lease is served.
    Hello {
        /// The client's [`PROTO_VERSION`]; a mismatch is refused, not guessed.
        proto: u32,
        /// The binary and version, for the log (`thegn/0.1.0`).
        client: String,
        /// Lowercase hex HMAC-SHA256(secret, server nonce ‖ client), proving the
        /// client owns the secret without sending it.
        mac: String,
    },
    /// Create (or reattach) a lease. The reply is a [`LeaseGrant`].
    Lease(LeaseSpec),
    /// Attach a sandbox process to its lease, so a leaked token dies with it.
    Bind {
        /// The lease to bind to.
        lease: LeaseId,
        /// The process id to watch.
        pid: u32,
        /// The process start time (Linux `/proc` field 22), to survive pid
        /// reuse.
        start_time: Option<u64>,
    },
    /// Replace a live lease's policy and phase, e.g. on `setup → agent`.
    Update {
        /// The lease to update.
        lease: LeaseId,
        /// The complete new policy; a patch, never a merge.
        policy: EgressPolicy,
        /// The new phase, when it changed.
        phase: Option<LeasePhase>,
        /// Refuse connections right now instead of draining open ones.
        close_now_denied: bool,
    },
    /// Drop a lease. Idempotent: releasing a gone lease succeeds.
    Release {
        /// The lease to release.
        lease: LeaseId,
    },
    /// List the live leases.
    List,
    /// Per-lease traffic counters.
    Stats,
    /// Subscribe this socket to a selection of pushes.
    Subscribe {
        /// Send [`ControlEvent::ApprovalRequested`].
        approvals: bool,
        /// Send [`ControlEvent::Decision`].
        decisions: bool,
        /// Send [`ControlEvent::LeaseGone`] for these leases; `None` means none.
        leases: Option<Vec<LeaseId>>,
        /// Send [`ControlEvent::Dropped`], the coalesced "n decisions were not
        /// sent" notice for a slow subscriber.
        keep: bool,
    },
    /// Answer one approval request.
    Approve {
        /// The id carried by the `approval_requested` event being answered.
        request: u64,
        /// Allow once, allow for the worktree, or deny.
        verdict: ApprovalVerdict,
        /// Who answered (`user`, `headless`, `timeout`), for the audit trail.
        by: String,
    },
    /// Drop every lease, close the listeners and exit.
    Shutdown,
}

/// Everything the proxy needs to serve one lease (WS04 §3.2).
///
/// The policy is resolved (presets folded in, overlays clamped) before it is
/// sent: the proxy never learns preset names and never re-derives a rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseSpec {
    /// Stable sandbox identity: worktree key plus pane or container.
    pub key: String,
    /// The worktree the lease belongs to, as an absolute path. Also the
    /// `AllowWorktree` approval scope.
    pub worktree: String,
    /// The resolved policy.
    pub policy: EgressPolicy,
    /// Who owns the lease: which pane, container or job it dies with.
    pub owner: LeaseOwner,
    /// How the sandbox reaches the proxy.
    pub transport: Transport,
    /// How much of the traffic is recorded. Whether an unmatched host is denied
    /// or asked about is [`EgressPolicy::approvals`], the one place it lives.
    #[serde(default)]
    pub audit: AuditLevel,
    /// The upstream HTTP/SOCKS5 proxy to chain to, when the sandbox cannot
    /// reach the internet directly.
    #[serde(default)]
    pub upstream: Option<Upstream>,
    /// The proxy's resource limits, reused verbatim from the nftables seed.
    pub limits: EgressProxyLimits,
    /// The lifecycle phase the lease is in; `setup` refuses `agent` hosts.
    #[serde(default)]
    pub phase: LeasePhase,
    /// Credentials the proxy injects for this lease. Only the service and the
    /// source name cross this message: a [`ResolvedCredential`] never serialises
    /// its value, so persisting a `LeaseSpec` cannot write a secret. The value
    /// reaches the proxy through the dedicated secret path (WS04-P5-02).
    #[serde(default)]
    pub credentials: Vec<ResolvedCredential>,
}

/// Who owns a lease; the lease dies with its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseOwner {
    /// A pane process; the lease is released when the pid exits.
    Pane {
        /// The pane's process id.
        pid: Option<u32>,
    },
    /// A container; the lease is released when the container stops.
    Container {
        /// The engine (`podman`, `docker`).
        engine: String,
        /// The engine's container name.
        name: String,
    },
    /// A background job; released when the job's process exits.
    Job {
        /// The job's process id.
        pid: Option<u32>,
    },
}

/// How the sandbox reaches the proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Transport {
    /// A unix socket in a directory bound into the sandbox.
    UnixDir,
    /// A loopback TCP listener. `port` reuses the previous port after a
    /// restart, so a long-lived process keeps its proxy URL.
    LoopbackTcp {
        /// The port to prefer; `None` lets the proxy choose.
        port: Option<u16>,
    },
    /// A relay reached by the sandbox rather than by the host (a remote or
    /// container sandbox).
    Relay,
}

/// The lease lifecycle phase (WS04-P5-06).
///
/// An absent phase is [`Self::Agent`], the plan's default: the setup gate exists
/// for a sandbox that is still bootstrapping, and a lease created for a running
/// sandbox has no bootstrap phase to protect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeasePhase {
    /// Bootstrap traffic only: package registries, the toolchain.
    Setup,
    /// The agent's traffic: everything the resolved policy allows.
    #[default]
    Agent,
}

/// An upstream proxy a lease chains to (WS04-P2-05).
///
/// The credentials are `SecretRef` names, never values: the value arrives as a
/// lease-scoped [`ResolvedCredential`] on the secret path (WS04-P5-02), so the
/// upstream's password is never stored in a lease record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// `http` or `socks5`.
    pub scheme: String,
    /// The upstream proxy host.
    pub host: String,
    /// The upstream proxy port.
    pub port: u16,
    /// The `SecretRef::audit_name()`s of the upstream's credentials.
    #[serde(default)]
    pub credentials: Vec<String>,
}

/// One credential the proxy injects for a lease: the service it authenticates
/// to, the [`SecretRef`](crate::secretref::SecretRef) it came from, and the
/// value.
///
/// `Debug` is redacted for the value (B13): the `secret_ref` is a scheme plus an
/// operand (see [`SecretRef::audit_name`](crate::secretref::SecretRef::audit_name))
/// and is safe to print, the value is not.
///
/// **The value is never serialised** (`skip_serializing`): anything that
/// persists or logs a [`LeaseSpec`] writes the service and the source name only.
/// It still deserialises when present, so the dedicated secret frame of
/// WS04-P5-02 can reuse the type; an absent value reads as the empty string.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCredential {
    /// The service the credential belongs to (`github`, `registry.npmjs.org`).
    pub service: String,
    /// The source the credential came from, as `SecretRef::audit_name()`.
    pub secret_ref: String,
    /// The value. Never logged, never audited, never serialised.
    #[serde(skip_serializing, default)]
    pub value: String,
}

impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("service", &self.service)
            .field("secret_ref", &self.secret_ref)
            .field("value", &REDACTED)
            .finish()
    }
}

impl ResolvedCredential {
    /// Build a credential from its service, source name and value.
    #[must_use]
    pub fn new(
        service: impl Into<String>,
        secret_ref: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            service: service.into(),
            secret_ref: secret_ref.into(),
            value: value.into(),
        }
    }
}

/// What [`ControlRequest::Lease`] answers: the lease's id, its proxy user name,
/// its bearer token and where the sandbox reaches the proxy.
///
/// Deliberately **not** `Serialize`: a lease record on disk stores only the
/// token's SHA-256 (WS04-P2-01b). The socket reply uses [`LeaseGrantWire`],
/// built with [`LeaseGrant::to_wire`]. `Debug` is redacted because it holds a
/// [`LeaseToken`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseGrant {
    /// The lease id, also the proxy user name (`lease-<hex16>`).
    pub id: LeaseId,
    /// The proxy user name the sandbox authenticates as.
    pub lease_user: String,
    /// The bearer token; `Debug` prints `LeaseToken(***)`.
    pub token: LeaseToken,
    /// Where the sandbox reaches the proxy.
    pub endpoint: ProxyEndpoint,
}

impl LeaseGrant {
    /// The frozen socket form of this grant. Only the authenticated control
    /// socket may carry the result; never persist or log it.
    #[must_use]
    pub fn to_wire(&self) -> LeaseGrantWire {
        LeaseGrantWire {
            id: self.id,
            lease_user: self.lease_user.clone(),
            token: self.token.expose_secret().to_string(),
            endpoint: EndpointWire::from(&self.endpoint),
        }
    }
}

/// The frozen wire shape of a [`LeaseGrant`]: the `ok` payload of a `lease`
/// reply.
///
/// `token` is the bearer token's plain text, so this is the one type whose
/// serialised form contains a secret: it may travel only over the authenticated
/// control socket (persistence stores SHA-256 instead). `Debug` redacts it and
/// the type has no `Display`; never log the serialised line.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrantWire {
    /// The lease id.
    pub id: LeaseId,
    /// The proxy user name.
    pub lease_user: String,
    /// The bearer token's text. Never logged.
    pub token: String,
    /// Where the sandbox reaches the proxy.
    pub endpoint: EndpointWire,
}

impl std::fmt::Debug for LeaseGrantWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseGrantWire")
            .field("id", &self.id)
            .field("lease_user", &self.lease_user)
            .field("token", &REDACTED)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl From<LeaseGrantWire> for LeaseGrant {
    fn from(wire: LeaseGrantWire) -> Self {
        Self {
            id: wire.id,
            lease_user: wire.lease_user,
            token: LeaseToken::new(wire.token),
            endpoint: ProxyEndpoint::from(wire.endpoint),
        }
    }
}

/// The wire form of a [`ProxyEndpoint`], tagged by `kind`: `unix_dir`,
/// `loopback` or `relay`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointWire {
    /// A unix-socket directory bound into the sandbox.
    UnixDir {
        /// The directory on the host.
        host_dir: PathBuf,
        /// Where it appears inside the sandbox.
        in_sandbox: PathBuf,
    },
    /// A loopback address.
    Loopback {
        /// The proxy's listening address (`127.0.0.1:3128`).
        addr: SocketAddr,
    },
    /// A relay reachable by host name and port.
    Relay {
        /// The relay host.
        host: String,
        /// The relay port.
        port: u16,
    },
}

impl From<&ProxyEndpoint> for EndpointWire {
    fn from(endpoint: &ProxyEndpoint) -> Self {
        match endpoint {
            ProxyEndpoint::UnixDir {
                host_dir,
                in_sandbox,
            } => Self::UnixDir {
                host_dir: host_dir.clone(),
                in_sandbox: in_sandbox.clone(),
            },
            ProxyEndpoint::Loopback { addr } => Self::Loopback { addr: *addr },
            ProxyEndpoint::Relay { host, port } => Self::Relay {
                host: host.clone(),
                port: *port,
            },
        }
    }
}

impl From<EndpointWire> for ProxyEndpoint {
    fn from(wire: EndpointWire) -> Self {
        match wire {
            EndpointWire::UnixDir {
                host_dir,
                in_sandbox,
            } => Self::UnixDir {
                host_dir,
                in_sandbox,
            },
            EndpointWire::Loopback { addr } => Self::Loopback { addr },
            EndpointWire::Relay { host, port } => Self::Relay { host, port },
        }
    }
}

/// The answer to one [`ControlEvent::ApprovalRequested`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalVerdict {
    /// Allow this connection only.
    AllowOnce,
    /// Allow this host for the lease's worktree, for the session.
    AllowWorktree,
    /// Refuse the connection.
    Deny,
}

/// The protocol a connection was carried over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    /// A `CONNECT` tunnel.
    Connect,
    /// Forward-proxied HTTP.
    Http,
    /// A SOCKS5 CONNECT.
    Socks5,
    /// A protocol a newer writer added; readers must not fail on it.
    #[serde(other)]
    Unknown,
}

/// What was decided about a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Allowed.
    Allow,
    /// Denied.
    Deny,
    /// An action a newer writer added; readers must not fail on it.
    #[serde(other)]
    Unknown,
}

/// Why a connection was denied; [`Self::None`] on an allow.
///
/// The first four names are the evaluator's own vocabulary (WS04 §3.3
/// precedence: hard floor, deny rule, IP literal, no rule), so the audit record
/// and `EgressDecision` never need a translation table. [`Self::Unknown`] is
/// what any unrecognised value reads as, which is what keeps v1 additive-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyReason {
    /// Not denied.
    None,
    /// A malformed host, or a metadata name (`metadata.google.internal`, …):
    /// the hard floor, never waivable.
    HardFloor,
    /// A `deny` pattern matched.
    Rule,
    /// The target was an IP literal and no `allow_ip` or `local` rule covers it.
    IpLiteral,
    /// No rule matched and approvals are off, or the prompt timed out.
    NotAllowed,
    /// The name resolved to an address in a guarded class that no `local` or
    /// `guard_allow` rule waives.
    Guarded,
    /// The lease is in the `setup` phase and the host is not a bootstrap host.
    Setup,
    /// `require_sni` is on and the TLS SNI did not equal the CONNECT host.
    SniMismatch,
    /// The proxy credentials were missing or wrong.
    AuthFailed,
    /// A lease limit (connections, bytes, lifetime) was exceeded.
    LimitExceeded,
    /// A reason a newer writer added; readers must not fail on it.
    #[serde(other)]
    Unknown,
}

/// One response line: the id echoed from the request, then either `ok` or
/// `err`.
///
/// Untagged in serde terms: the presence of `ok` versus `err` selects the
/// variant, which is exactly the documented shape. An `ok` payload is a
/// [`serde_json::Value`] because the operation that answers decides its type
/// (`null`, [`HelloReply`], [`LeaseGrant`] fields, the lease list, the stats
/// map); freezing every payload as a struct here would freeze the proxy's
/// internals too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlResponse {
    /// The operation succeeded.
    Ok {
        /// The request id being answered.
        id: u64,
        /// The operation's payload.
        ok: serde_json::Value,
    },
    /// The operation failed.
    Err {
        /// The request id being answered.
        id: u64,
        /// The stable code and human message.
        err: WireError,
    },
}

/// The `err` payload of [`ControlResponse::Err`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    /// A stable machine-readable code: `bad_request`, `unknown_op`,
    /// `not_authed`, `no_lease`, `shutting_down`.
    pub code: String,
    /// The human message, already redacted: never a token, never a header.
    pub msg: String,
}

/// The `ok` payload of a [`ControlRequest::Hello`] response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    /// The proxy's [`PROTO_VERSION`]; a client refuses a mismatch.
    pub proto: u32,
    /// The proxy binary's git sha, for the log.
    pub git_sha: String,
    /// The optional features the proxy implements, so an older client degrades
    /// instead of failing.
    #[serde(default)]
    pub features: Vec<String>,
}

/// A server push (WS04 §3.2). Events carry no `id`: they answer no request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ControlEvent {
    /// The first line the server sends on a new connection: the single-use nonce
    /// the client's `hello` MAC is computed over (see the module doc).
    Challenge {
        /// The server's [`PROTO_VERSION`].
        proto: u32,
        /// Lowercase hex, at least 16 random bytes, never reused.
        nonce: String,
    },
    /// A connection needs a human verdict; the proxy holds it without
    /// resolving DNS until [`ControlRequest::Approve`] or the timeout.
    ApprovalRequested {
        /// The id to answer with [`ControlRequest::Approve`].
        request: u64,
        /// The lease the connection belongs to.
        lease: LeaseId,
        /// The worktree, so the UI can scope an `AllowWorktree` verdict.
        worktree: String,
        /// The host that needs a verdict.
        host: String,
        /// The port that needs a verdict.
        port: u16,
        /// The protocol the connection arrived on.
        proto: Proto,
        /// Why no rule allowed it, for the UI.
        reason: Option<String>,
    },
    /// An approval request was answered (by a human, by the timeout, or by a
    /// headless auto-deny).
    ApprovalResolved {
        /// The request id that was answered.
        request: u64,
        /// What was decided.
        verdict: ApprovalVerdict,
        /// Who decided (`user`, `headless`, `timeout`).
        by: String,
    },
    /// One audit record, also written to the sink when the lease's audit level
    /// asks for it.
    Decision(AuditRecord),
    /// `n` events were not sent to this subscriber because it fell behind.
    Dropped {
        /// The number of dropped events, coalesced into one notice.
        n: u64,
    },
    /// A leased process exited or the lease was released.
    LeaseGone {
        /// The lease that is gone.
        lease: LeaseId,
        /// Why it went away.
        why: LeaseGoneReason,
    },
    /// An event a newer writer added; a reader skips it.
    #[serde(other)]
    Unknown,
}

impl ControlEvent {
    /// Clamp the peer-chosen text of an event to its byte bounds (B16): the
    /// host and reason of an approval request and the fields of a decision.
    /// Consumers call this after reading an event from the socket.
    #[must_use]
    pub fn bounded(self) -> Self {
        match self {
            Self::ApprovalRequested {
                request,
                lease,
                worktree,
                host,
                port,
                proto,
                reason,
            } => Self::ApprovalRequested {
                request,
                lease,
                worktree,
                host: truncate_bytes(&host, MAX_AUDIT_HOST_LEN),
                port,
                proto,
                reason: reason.map(|reason| truncate_bytes(&reason, MAX_APPROVAL_REASON_LEN)),
            },
            Self::Decision(record) => Self::Decision(record.bounded()),
            other => other,
        }
    }
}

/// Why a lease is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseGoneReason {
    /// The owner process exited.
    OwnerExited,
    /// The owner container stopped.
    ContainerStopped,
    /// The client released it.
    Released,
    /// The proxy shut down.
    Shutdown,
    /// The lease expired on its own.
    Expired,
    /// A reason a newer writer added; readers must not fail on it.
    #[serde(other)]
    Unknown,
}

/// One audit record, schema `v: 1` (WS04 §3.2, resolution B15).
///
/// This is a **published format**: a record may be replayed, diffed and
/// grepped, so a field's bytes never depend on terminal width, locale or theme.
/// Schema v1 is consumed by `thegn sandbox denials` (WS11-P4-05) and read as
/// gate-4 canary evidence (THE-791), which is why a field may only be added, not
/// renamed or removed.
///
/// Field meanings:
/// - `v` is [`AUDIT_SCHEMA_VERSION`]; readers accept any `v` and ignore fields
///   they do not know.
/// - `ts` is UTC RFC 3339 with milliseconds (`2026-11-23T10:14:05.123Z`).
/// - `lease` and `worktree` identify the lease; `worktree` is repeated so a
///   record survives the lease's removal.
/// - `proto`, `decision` and `reason` are the frozen string vocabularies
///   ([`Proto`], [`Action`], [`DenyReason`]).
/// - `ip` is the address the name resolved to, or `None` when nothing was
///   resolved (a denied host, a guard refusal).
/// - `sni` is what the client asked for in the TLS handshake; `method` and
///   `path` exist only for intercepted HTTP, and `path` never carries the query
///   string.
/// - `rule` names the rule that decided (`allow:crates.io`,
///   `deny:*.evil.example`), when one did.
/// - `bytes_up`/`bytes_down`/`dur_ms` are the tunnel's totals; zero when the
///   connection was refused before any byte moved.
/// - `secret_id` names the credential used, by service, never its value.
///
/// `host`, `sni`, `method` and `path` come from the peer and are bounded by
/// [`AuditRecord::bounded`] before they are stored (B16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// [`AUDIT_SCHEMA_VERSION`].
    pub v: u8,
    /// UTC RFC 3339 timestamp with milliseconds.
    pub ts: String,
    /// The lease the connection belonged to.
    pub lease: LeaseId,
    /// The lease's worktree, absolute.
    pub worktree: String,
    /// The protocol the connection arrived on.
    pub proto: Proto,
    /// The host as the client named it.
    pub host: String,
    /// The port the client asked for.
    pub port: u16,
    /// The address the host resolved to, when it resolved.
    pub ip: Option<IpAddr>,
    /// The SNI the client presented, when it presented one.
    pub sni: Option<String>,
    /// The HTTP method, for an intercepted request.
    pub method: Option<String>,
    /// The request target's path only, never the query, for an intercepted
    /// request.
    pub path: Option<String>,
    /// What was decided.
    pub decision: Action,
    /// Why.
    pub reason: DenyReason,
    /// The rule that decided, when one did.
    pub rule: Option<String>,
    /// Bytes sent from the sandbox toward the destination.
    pub bytes_up: u64,
    /// Bytes sent from the destination toward the sandbox.
    pub bytes_down: u64,
    /// The connection's lifetime in milliseconds.
    pub dur_ms: u64,
    /// The service whose credential was injected, when one was.
    pub secret_id: Option<String>,
}

impl AuditRecord {
    /// Clamp the peer-chosen fields to their byte bounds (B16) and cut the path
    /// at the first `?` or `#`, so a query string never reaches a record.
    ///
    /// Consumers that read a record from a file or the socket call this too.
    ///
    /// Producers call this once, when the record is built; a renderer still
    /// escapes the stored text, so a host that is 253 bytes of control
    /// characters cannot rewrite a log line. Truncation never splits a UTF-8
    /// character.
    #[must_use]
    pub fn bounded(mut self) -> Self {
        self.host = truncate_bytes(&self.host, MAX_AUDIT_HOST_LEN);
        self.sni = self.sni.map(|sni| truncate_bytes(&sni, MAX_AUDIT_SNI_LEN));
        self.method = self
            .method
            .map(|method| truncate_bytes(&method, MAX_AUDIT_METHOD_LEN));
        self.path = self.path.map(|path| {
            let end = path.find(['?', '#']).unwrap_or(path.len());
            truncate_bytes(&path[..end], MAX_AUDIT_PATH_LEN)
        });
        self
    }
}

/// Truncate `value` to at most `max` bytes without splitting a UTF-8 character.
fn truncate_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// The number of bytes in a [`LeaseId`].
const LEASE_ID_BYTES: usize = 16;

/// The number of hex characters a [`LeaseId`] has on the wire.
const LEASE_ID_HEX: usize = LEASE_ID_BYTES * 2;

/// Lowercase hex digits, the canonical spelling of a [`LeaseId`].
const HEX_DIGITS: [u8; 16] = *b"0123456789abcdef";

/// A [`LeaseId`] is a lowercase hex string on the wire (`32` characters), so a
/// lease id in a log, a socket name and a JSON record are the same text.
impl Serialize for LeaseId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&lease_id_hex(*self))
    }
}

/// Accepts upper or lower case on input and canonicalises to lowercase.
impl<'de> Deserialize<'de> for LeaseId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match lease_id_from_hex(&raw) {
            Some(id) => Ok(id),
            None => Err(serde::de::Error::custom(format!(
                "lease id must be {LEASE_ID_HEX} hex characters, got {raw:?}"
            ))),
        }
    }
}

/// The canonical text of a lease id.
fn lease_id_hex(id: LeaseId) -> String {
    let mut out = String::with_capacity(LEASE_ID_HEX);
    for byte in id.0 {
        out.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Parse a lease id, rejecting anything that is not exactly 32 hex characters.
fn lease_id_from_hex(raw: &str) -> Option<LeaseId> {
    let bytes = raw.as_bytes();
    if bytes.len() != LEASE_ID_HEX {
        return None;
    }
    let mut id = [0u8; LEASE_ID_BYTES];
    for (index, slot) in id.iter_mut().enumerate() {
        let high = hex_value(bytes[index * 2])?;
        let low = hex_value(bytes[index * 2 + 1])?;
        *slot = (high << 4) | low;
    }
    Some(LeaseId(id))
}

/// One hex digit, accepting either case.
const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The serde form of [`EgressProxyLimits`]: the same field names as
/// `[sandbox.egress.limits]`.
///
/// [`EgressProxyLimits`] lives in [`crate::egress_nft`] (the nftables seed) and
/// is reused verbatim; its serde impls are here so this item does not edit that
/// module.
#[derive(Serialize, Deserialize)]
struct ProxyLimitsWire {
    connect_timeout_ms: u64,
    header_timeout_ms: u64,
    idle_timeout_ms: u64,
    max_lifetime_ms: u64,
    max_connections: usize,
    max_bytes_per_direction: u64,
}

impl From<&EgressProxyLimits> for ProxyLimitsWire {
    fn from(limits: &EgressProxyLimits) -> Self {
        Self {
            connect_timeout_ms: limits.connect_timeout_ms,
            header_timeout_ms: limits.header_timeout_ms,
            idle_timeout_ms: limits.idle_timeout_ms,
            max_lifetime_ms: limits.max_lifetime_ms,
            max_connections: limits.max_connections,
            max_bytes_per_direction: limits.max_bytes_per_direction,
        }
    }
}

impl From<ProxyLimitsWire> for EgressProxyLimits {
    fn from(wire: ProxyLimitsWire) -> Self {
        Self {
            connect_timeout_ms: wire.connect_timeout_ms,
            header_timeout_ms: wire.header_timeout_ms,
            idle_timeout_ms: wire.idle_timeout_ms,
            max_lifetime_ms: wire.max_lifetime_ms,
            max_connections: wire.max_connections,
            max_bytes_per_direction: wire.max_bytes_per_direction,
        }
    }
}

impl Serialize for EgressProxyLimits {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ProxyLimitsWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EgressProxyLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let limits = EgressProxyLimits::from(ProxyLimitsWire::deserialize(deserializer)?);
        limits.validate().map_err(serde::de::Error::custom)?;
        Ok(limits)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::egress::pattern::HostPattern;
    use crate::egress::policy::{EgressGuardClass, IpRule, LocalRule};

    const CHALLENGE: &str = include_str!("../../tests/fixtures/egress/challenge.json");
    const LEASE_GRANT: &str = include_str!("../../tests/fixtures/egress/lease_grant.json");
    const HELLO: &str = include_str!("../../tests/fixtures/egress/hello.json");
    const LEASE: &str = include_str!("../../tests/fixtures/egress/lease.json");
    const APPROVAL_REQUESTED: &str =
        include_str!("../../tests/fixtures/egress/approval_requested.json");
    const AUDIT_RECORD: &str = include_str!("../../tests/fixtures/egress/audit_record.json");

    const LEASE_ID: LeaseId = LeaseId([7; 16]);

    fn hello_line() -> ControlRequestLine {
        ControlRequestLine {
            id: 1,
            request: ControlRequest::Hello {
                proto: PROTO_VERSION,
                client: "thegn/0.1.0".into(),
                mac: "5f2d1c0a9b8e7d6c5b4a39281706f5e4d3c2b1a0918273645546372819a0b1c2".into(),
            },
        }
    }

    fn lease_spec() -> LeaseSpec {
        LeaseSpec {
            key: "thegn:alpha/wt-0707/pane-3".into(),
            worktree: "/home/u/src/thegn-alpha".into(),
            policy: EgressPolicy {
                allow: vec![
                    HostPattern::parse("github.com").expect("valid pattern"),
                    HostPattern::parse("*.githubusercontent.com").expect("valid pattern"),
                    HostPattern::parse("crates.io").expect("valid pattern"),
                ],
                deny: vec![HostPattern::parse("*.evil.example").expect("valid pattern")],
                allow_ip: vec![IpRule {
                    cidr: "203.0.113.0/24".into(),
                    port: Some(443),
                }],
                local: vec![LocalRule {
                    host: "host.thegn.internal".into(),
                    port: 3000,
                }],
                guard_allow: vec![EgressGuardClass::Loopback, EgressGuardClass::Private],
                ports: vec![443, 80],
                tls_ports: vec![443],
                require_sni: true,
                approvals: ApprovalMode::Prompt,
            },
            owner: LeaseOwner::Container {
                engine: "podman".into(),
                name: "thegn-wt-0707".into(),
            },
            transport: Transport::LoopbackTcp { port: Some(3128) },
            audit: AuditLevel::Decisions,
            upstream: Some(Upstream {
                scheme: "http".into(),
                host: "proxy.corp.example".into(),
                port: 8080,
                credentials: vec!["keyring:corp-proxy".into()],
            }),
            limits: EgressProxyLimits::default(),
            phase: LeasePhase::Agent,
            // The value never serialises, so the golden lease round-trips with
            // an empty one.
            credentials: vec![ResolvedCredential::new("github", "keyring:gh-token", "")],
        }
    }

    fn approval_requested_event() -> ControlEvent {
        ControlEvent::ApprovalRequested {
            request: 7,
            lease: LEASE_ID,
            worktree: "/home/u/src/thegn-alpha".into(),
            host: "registry.npmjs.org".into(),
            port: 443,
            proto: Proto::Connect,
            reason: Some("development@1 does not cover this host".into()),
        }
    }

    fn audit_record_value() -> AuditRecord {
        AuditRecord {
            v: AUDIT_SCHEMA_VERSION,
            ts: "2026-11-23T10:14:05.123Z".into(),
            lease: LEASE_ID,
            worktree: "/home/u/src/thegn-alpha".into(),
            proto: Proto::Http,
            host: "crates.io".into(),
            port: 443,
            ip: Some(IpAddr::from([198, 51, 100, 7])),
            sni: Some("crates.io".into()),
            method: Some("GET".into()),
            path: Some("/api/v1/crates".into()),
            decision: Action::Allow,
            reason: DenyReason::None,
            rule: Some("allow:crates.io".into()),
            bytes_up: 1234,
            bytes_down: 5678,
            dur_ms: 321,
            secret_id: None,
        }
    }

    /// Parse `fixture`, compare it to `expected` and re-serialise it back to the
    /// same bytes: the golden files are the wire, not an example of it.
    fn fixture_roundtrip<T>(fixture: &str, expected: T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        assert!(fixture.ends_with('\n'), "a fixture ends with one newline");
        assert_eq!(
            fixture.matches('\n').count(),
            1,
            "a fixture is exactly one NDJSON line"
        );
        let parsed: T = serde_json::from_str(fixture).expect("fixture parses");
        assert_eq!(parsed, expected);
        let rendered = serde_json::to_string(&parsed).expect("value serialises");
        assert_eq!(
            format!("{rendered}\n"),
            fixture,
            "fixture is the canonical serde form"
        );
    }

    #[test]
    fn challenge_fixture_is_stable() {
        fixture_roundtrip(
            CHALLENGE,
            ControlEvent::Challenge {
                proto: PROTO_VERSION,
                nonce: "9c1e4f7a2b6d8035e1f4a7c9b2d6e803".into(),
            },
        );
    }

    #[test]
    fn lease_grant_fixture_is_stable() {
        let grant = LeaseGrant {
            id: LEASE_ID,
            lease_user: "lease-0707".into(),
            token: LeaseToken::new("lease-token-example".into()),
            endpoint: ProxyEndpoint::Loopback {
                addr: SocketAddr::from(([127, 0, 0, 1], 3128)),
            },
        };
        fixture_roundtrip(LEASE_GRANT, grant.to_wire());
        let wire: LeaseGrantWire = serde_json::from_str(LEASE_GRANT).expect("grant parses");
        assert_eq!(LeaseGrant::from(wire.clone()), grant);
        let rendered = format!("{wire:?}");
        assert!(!rendered.contains("lease-token-example"), "{rendered}");

        for endpoint in [
            ProxyEndpoint::UnixDir {
                host_dir: PathBuf::from("/run/thegn/egress/v1/0707"),
                in_sandbox: PathBuf::from("/run/egress"),
            },
            ProxyEndpoint::Relay {
                host: "relay.example".into(),
                port: 8443,
            },
        ] {
            let json = serde_json::to_string(&EndpointWire::from(&endpoint)).expect("serialises");
            let back: EndpointWire = serde_json::from_str(&json).expect("parses");
            assert_eq!(ProxyEndpoint::from(back), endpoint);
        }
    }

    #[test]
    fn unknown_vocabulary_values_decode_to_unknown() {
        let mut record = serde_json::to_value(audit_record_value()).expect("record serialises");
        let object = record.as_object_mut().expect("record is an object");
        object.insert("reason".into(), serde_json::json!("quota_v1_1"));
        object.insert("proto".into(), serde_json::json!("quic"));
        object.insert("decision".into(), serde_json::json!("challenge"));
        let parsed: AuditRecord = serde_json::from_value(record).expect("unknown values decode");
        assert_eq!(parsed.reason, DenyReason::Unknown);
        assert_eq!(parsed.proto, Proto::Unknown);
        assert_eq!(parsed.decision, Action::Unknown);

        let gone: ControlEvent = serde_json::from_str(
            r#"{"event":"lease_gone","lease":"07070707070707070707070707070707","why":"evicted"}"#,
        )
        .expect("unknown why decodes");
        assert_eq!(
            gone,
            ControlEvent::LeaseGone {
                lease: LEASE_ID,
                why: LeaseGoneReason::Unknown,
            }
        );

        let future: ControlEvent =
            serde_json::from_str(r#"{"event":"quota","n":3}"#).expect("unknown event decodes");
        assert_eq!(future, ControlEvent::Unknown);

        for (reason, wire) in [
            (DenyReason::SniMismatch, "sni_mismatch"),
            (DenyReason::AuthFailed, "auth_failed"),
            (DenyReason::LimitExceeded, "limit_exceeded"),
        ] {
            assert_eq!(
                serde_json::to_string(&reason).expect("serialises"),
                format!("\"{wire}\"")
            );
        }
    }

    #[test]
    fn resolved_credential_never_serialises_its_value() {
        let credential = ResolvedCredential::new("github", "keyring:gh-token", "ghp_secretValue");
        let json = serde_json::to_string(&credential).expect("serialises");
        assert!(!json.contains("ghp_secretValue"), "{json}");
        assert_eq!(
            json,
            r#"{"service":"github","secret_ref":"keyring:gh-token"}"#
        );
        let back: ResolvedCredential = serde_json::from_str(&json).expect("parses");
        assert_eq!(back.value, "");
    }

    #[test]
    fn audit_path_drops_the_query_and_fragment() {
        let mut record = audit_record_value();
        record.path = Some("/api/v1/crates?token=abc#frag".into());
        assert_eq!(record.bounded().path.as_deref(), Some("/api/v1/crates"));
    }

    #[test]
    fn approval_request_peer_text_is_bounded() {
        let event = ControlEvent::ApprovalRequested {
            request: 1,
            lease: LEASE_ID,
            worktree: "/w".into(),
            host: "h".repeat(MAX_AUDIT_HOST_LEN + 10),
            port: 443,
            proto: Proto::Connect,
            reason: Some("r".repeat(MAX_APPROVAL_REASON_LEN + 10)),
        };
        let ControlEvent::ApprovalRequested { host, reason, .. } = event.bounded() else {
            panic!("variant changed");
        };
        assert_eq!(host.len(), MAX_AUDIT_HOST_LEN);
        assert_eq!(reason.map(|r| r.len()), Some(MAX_APPROVAL_REASON_LEN));
    }

    #[test]
    fn invalid_limits_are_a_wire_error() {
        let mut value = serde_json::to_value(EgressProxyLimits::default()).expect("limits");
        value["max_connections"] = serde_json::json!(0);
        assert!(serde_json::from_value::<EgressProxyLimits>(value).is_err());
        let mut value = serde_json::to_value(EgressProxyLimits::default()).expect("limits");
        value["idle_timeout_ms"] = serde_json::json!(u64::MAX);
        assert!(serde_json::from_value::<EgressProxyLimits>(value).is_err());
    }

    #[test]
    fn crlf_line_at_the_cap_is_accepted() {
        let mut ok = vec![b'a'; MAX_LINE];
        ok.extend_from_slice(b"\r\n");
        assert_eq!(
            read_line_bounded(&mut Cursor::new(ok), MAX_LINE)
                .expect("CRLF line at the cap")
                .len(),
            MAX_LINE
        );
        let mut over = vec![b'a'; MAX_LINE + 1];
        over.extend_from_slice(b"\r\n");
        assert!(read_line_bounded(&mut Cursor::new(over), MAX_LINE).is_err());
    }

    #[test]
    fn hello_fixture_is_stable() {
        fixture_roundtrip(HELLO, hello_line());
    }

    #[test]
    fn lease_fixture_is_stable() {
        fixture_roundtrip(
            LEASE,
            ControlRequestLine {
                id: 2,
                request: ControlRequest::Lease(lease_spec()),
            },
        );
    }

    #[test]
    fn approval_requested_fixture_is_stable() {
        fixture_roundtrip(APPROVAL_REQUESTED, approval_requested_event());
    }

    #[test]
    fn audit_record_fixture_is_stable() {
        fixture_roundtrip(AUDIT_RECORD, audit_record_value());
    }

    #[test]
    fn every_request_variant_roundtrips_with_its_id() {
        let requests = [
            hello_line().request,
            ControlRequest::Lease(lease_spec()),
            ControlRequest::Bind {
                lease: LeaseId([1; 16]),
                pid: 4242,
                start_time: Some(1_700_000_000),
            },
            ControlRequest::Update {
                lease: LeaseId([2; 16]),
                policy: EgressPolicy::default(),
                phase: Some(LeasePhase::Agent),
                close_now_denied: true,
            },
            ControlRequest::Release {
                lease: LeaseId([3; 16]),
            },
            ControlRequest::List,
            ControlRequest::Stats,
            ControlRequest::Subscribe {
                approvals: true,
                decisions: false,
                leases: Some(vec![LeaseId([4; 16])]),
                keep: true,
            },
            ControlRequest::Approve {
                request: 7,
                verdict: ApprovalVerdict::AllowWorktree,
                by: "user".into(),
            },
            ControlRequest::Shutdown,
        ];
        for (index, request) in requests.into_iter().enumerate() {
            let id = index as u64;
            let line = ControlRequestLine { id, request };
            let json = serde_json::to_string(&line).expect("request serialises");
            let value: serde_json::Value = serde_json::from_str(&json).expect("request is json");
            assert_eq!(value["id"], id);
            assert!(value["op"].is_string(), "no op tag: {json}");
            let back: ControlRequestLine = serde_json::from_str(&json).expect("request parses");
            assert_eq!(back, line);
        }
    }

    #[test]
    fn every_event_variant_roundtrips_without_an_id() {
        let events = [
            ControlEvent::Challenge {
                proto: PROTO_VERSION,
                nonce: "00ff".into(),
            },
            ControlEvent::Unknown,
            approval_requested_event(),
            ControlEvent::ApprovalResolved {
                request: 7,
                verdict: ApprovalVerdict::Deny,
                by: "timeout".into(),
            },
            ControlEvent::Decision(audit_record_value()),
            ControlEvent::Dropped { n: 12 },
            ControlEvent::LeaseGone {
                lease: LEASE_ID,
                why: LeaseGoneReason::OwnerExited,
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).expect("event serialises");
            let value: serde_json::Value = serde_json::from_str(&json).expect("event is json");
            assert!(value["event"].is_string(), "no event tag: {json}");
            assert!(value.get("id").is_none(), "an event carried an id: {json}");
            let back: ControlEvent = serde_json::from_str(&json).expect("event parses");
            assert_eq!(back, event);
        }

        // The `Decision` variant flattens the record into the event object, so a
        // subscriber reads one object and the sink reads the same fields.
        let decision = serde_json::to_value(ControlEvent::Decision(audit_record_value()))
            .expect("decision serialises");
        assert_eq!(decision["event"], "decision");
        assert_eq!(decision["v"], AUDIT_SCHEMA_VERSION);
        assert_eq!(decision["decision"], "allow");
        assert_eq!(decision["proto"], "http");
    }

    #[test]
    fn unknown_fields_ignored() {
        // A v1.1 client may send a field v1 does not know; the line still
        // parses, which is what makes additive changes safe.
        let line: ControlRequestLine =
            serde_json::from_str(r#"{"id":9,"op":"list","future_kind":"x"}"#)
                .expect("an unknown top-level field is ignored");
        assert_eq!(
            line,
            ControlRequestLine {
                id: 9,
                request: ControlRequest::List,
            }
        );

        let mut record = serde_json::to_value(audit_record_value()).expect("record serialises");
        let object = record.as_object_mut().expect("record is an object");
        object.insert("agent".into(), serde_json::json!("claude@1"));
        let parsed: AuditRecord = serde_json::from_value(record).expect("additive field ignored");
        assert_eq!(parsed, audit_record_value());
    }

    #[test]
    fn oversize_line_rejected() {
        let mut at_cap = Cursor::new(vec![b'a'; MAX_LINE]);
        assert_eq!(
            read_line_bounded(&mut at_cap, MAX_LINE)
                .expect("a line of exactly MAX_LINE bytes")
                .len(),
            MAX_LINE
        );

        let mut over = Cursor::new(vec![b'a'; MAX_LINE + 1]);
        assert_eq!(
            read_line_bounded(&mut over, MAX_LINE)
                .expect_err("an oversized line is an error")
                .kind(),
            io::ErrorKind::InvalidData
        );

        // A reader that never ends: the cap stops the buffering, it does not
        // read to the end of the stream first.
        let mut endless = EndlessLine { served: 0 };
        assert_eq!(
            read_line_bounded(&mut endless, MAX_LINE)
                .expect_err("a runaway line is an error")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            endless.served <= MAX_LINE + ENDLESS_CHUNK.len(),
            "buffered {} bytes for a {MAX_LINE}-byte cap",
            endless.served
        );
    }

    #[test]
    fn read_line_strips_crlf_and_requires_utf8() {
        let mut crlf = Cursor::new(b"{\"id\":1}\r\n{\"id\":2}".to_vec());
        assert_eq!(
            read_line_bounded(&mut crlf, MAX_LINE).expect("first line"),
            "{\"id\":1}"
        );
        assert_eq!(
            read_line_bounded(&mut crlf, MAX_LINE).expect("second line"),
            "{\"id\":2}"
        );
        assert_eq!(
            read_line_bounded(&mut crlf, MAX_LINE)
                .expect_err("nothing left")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        let mut bad = Cursor::new(vec![0xff, b'\n']);
        assert_eq!(
            read_line_bounded(&mut bad, MAX_LINE)
                .expect_err("invalid utf-8")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn lease_token_never_serialised_in_debug() {
        const TOKEN: &str = "lease-secret-9d2f";
        const VALUE: &str = "ghp_credentialValue";
        let grant = LeaseGrant {
            id: LEASE_ID,
            lease_user: "lease-0707".into(),
            token: LeaseToken::new(TOKEN.into()),
            endpoint: ProxyEndpoint::Loopback {
                addr: SocketAddr::from(([127, 0, 0, 1], 3128)),
            },
        };
        let rendered = format!("{grant:?}");
        assert!(
            !rendered.contains(TOKEN),
            "LeaseGrant Debug leaked the token"
        );
        assert!(rendered.contains("LeaseToken(***)"));

        let credential = ResolvedCredential::new("github", "keyring:gh-token", VALUE);
        let rendered = format!("{credential:?}");
        assert!(
            !rendered.contains(VALUE),
            "ResolvedCredential Debug leaked the value"
        );
        assert!(rendered.contains("keyring:gh-token"));

        // The audit record has no field a secret value could be stored in.
        let rendered = serde_json::to_string(&audit_record_value()).expect("record serialises");
        assert!(rendered.contains("\"secret_id\":null"));
        assert!(!rendered.contains(VALUE));
    }

    #[test]
    fn response_lines_have_the_documented_shape() {
        let ok = ControlResponse::Ok {
            id: 3,
            ok: serde_json::to_value(HelloReply {
                proto: PROTO_VERSION,
                git_sha: "eefb04b29".into(),
                features: vec!["approvals".into()],
            })
            .expect("reply serialises"),
        };
        let json = serde_json::to_string(&ok).expect("ok serialises");
        // Compare as JSON values: `serde_json::Value` objects sort their keys
        // (no `preserve_order` feature), and key order carries no meaning on the wire.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).expect("ok is json"),
            serde_json::json!({"id":3,"ok":{"proto":1,"git_sha":"eefb04b29","features":["approvals"]}})
        );
        assert_eq!(
            serde_json::from_str::<ControlResponse>(&json).expect("ok parses"),
            ok
        );

        let err = ControlResponse::Err {
            id: 4,
            err: WireError {
                code: "unknown_op".into(),
                msg: "no such op".into(),
            },
        };
        let json = serde_json::to_string(&err).expect("err serialises");
        assert_eq!(
            json,
            r#"{"id":4,"err":{"code":"unknown_op","msg":"no such op"}}"#
        );
        assert_eq!(
            serde_json::from_str::<ControlResponse>(&json).expect("err parses"),
            err
        );

        let empty: ControlResponse =
            serde_json::from_str(r#"{"id":5,"ok":null}"#).expect("null ok parses");
        assert_eq!(
            empty,
            ControlResponse::Ok {
                id: 5,
                ok: serde_json::Value::Null,
            }
        );
    }

    #[test]
    fn lease_id_is_lowercase_hex_on_the_wire() {
        let id = LeaseId([0xab; 16]);
        let json = serde_json::to_string(&id).expect("lease id serialises");
        assert_eq!(json, format!("\"{}\"", "ab".repeat(16)));
        assert_eq!(serde_json::from_str::<LeaseId>(&json).expect("parses"), id);

        let upper = format!("\"{}\"", "AB".repeat(16));
        assert_eq!(
            serde_json::from_str::<LeaseId>(&upper).expect("upper case parses"),
            id,
            "case is accepted on input and canonicalised on output"
        );

        for bad in [
            "\"ab\"",
            "\"0707070707070707070707070707070\"",
            "\"07070707070707070707070707070707ff\"",
            "\"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\"",
        ] {
            assert!(
                serde_json::from_str::<LeaseId>(bad).is_err(),
                "{bad} parsed as a lease id"
            );
        }
    }

    #[test]
    fn audit_record_bounds_peer_fields() {
        let record = AuditRecord {
            v: AUDIT_SCHEMA_VERSION,
            ts: "2026-11-23T10:14:05.123Z".into(),
            lease: LEASE_ID,
            worktree: "/home/u/src/thegn-alpha".into(),
            proto: Proto::Connect,
            host: "h".repeat(MAX_AUDIT_HOST_LEN + 32),
            port: 443,
            ip: None,
            sni: Some("s".repeat(MAX_AUDIT_SNI_LEN + 1)),
            method: Some("M".repeat(MAX_AUDIT_METHOD_LEN + 4)),
            path: Some("p".repeat(MAX_AUDIT_PATH_LEN + 1)),
            decision: Action::Deny,
            reason: DenyReason::NotAllowed,
            rule: None,
            bytes_up: 0,
            bytes_down: 0,
            dur_ms: 1,
            secret_id: None,
        };
        let bounded = record.bounded();
        assert_eq!(bounded.host.len(), MAX_AUDIT_HOST_LEN);
        assert_eq!(
            bounded.sni.as_deref().map(str::len),
            Some(MAX_AUDIT_SNI_LEN)
        );
        assert_eq!(
            bounded.method.as_deref().map(str::len),
            Some(MAX_AUDIT_METHOD_LEN)
        );
        assert_eq!(
            bounded.path.as_deref().map(str::len),
            Some(MAX_AUDIT_PATH_LEN)
        );

        // A multibyte character straddling the bound is dropped whole, never
        // split: the stored text is always valid UTF-8.
        let split = AuditRecord {
            path: Some("p".repeat(MAX_AUDIT_PATH_LEN - 1) + "\u{e9}"),
            ..bounded
        }
        .bounded();
        assert_eq!(
            split.path.as_deref().map(str::len),
            Some(MAX_AUDIT_PATH_LEN - 1)
        );
        assert_eq!(
            split.path.as_deref(),
            Some("p".repeat(MAX_AUDIT_PATH_LEN - 1).as_str())
        );
    }

    #[test]
    fn lease_defaults_match_the_documented_shape() {
        let json = serde_json::json!({
            "id": 1,
            "op": "lease",
            "key": "thegn:alpha/wt-0707/pane-3",
            "worktree": "/home/u/src/thegn-alpha",
            "policy": {
                "allow": [],
                "deny": [],
                "allow_ip": [],
                "local": [],
                "guard_allow": [],
                "ports": [],
                "tls_ports": [],
                "require_sni": false,
                "approvals": "off"
            },
            "owner": { "kind": "job", "pid": null },
            "transport": { "kind": "relay" },
            "limits": serde_json::to_value(EgressProxyLimits::default()).expect("limits"),
        });
        let line: ControlRequestLine = serde_json::from_value(json).expect("lease parses");
        let ControlRequest::Lease(spec) = line.request else {
            panic!("expected a lease request");
        };
        assert_eq!(
            spec.phase,
            LeasePhase::Agent,
            "an absent phase is the plan's `agent` default"
        );
        assert_eq!(spec.policy.approvals, ApprovalMode::Off);
        assert_eq!(spec.audit, AuditLevel::Off);
        assert_eq!(spec.upstream, None);
        assert!(spec.credentials.is_empty());
    }

    #[test]
    fn proxy_limits_roundtrip() {
        let limits = EgressProxyLimits {
            connect_timeout_ms: 1_500,
            header_timeout_ms: 2_500,
            idle_timeout_ms: 60_000,
            max_lifetime_ms: 90_000,
            max_connections: 8,
            max_bytes_per_direction: 4_294_967_296,
        };
        let json = serde_json::to_string(&limits).expect("limits serialise");
        assert_eq!(
            json,
            r#"{"connect_timeout_ms":1500,"header_timeout_ms":2500,"idle_timeout_ms":60000,"max_lifetime_ms":90000,"max_connections":8,"max_bytes_per_direction":4294967296}"#
        );
        assert_eq!(
            serde_json::from_str::<EgressProxyLimits>(&json).expect("limits parse"),
            limits
        );
        assert_eq!(
            serde_json::to_string(&EgressProxyLimits::default()).expect("defaults serialise"),
            r#"{"connect_timeout_ms":10000,"header_timeout_ms":5000,"idle_timeout_ms":120000,"max_lifetime_ms":3600000,"max_connections":64,"max_bytes_per_direction":1073741824}"#
        );
    }

    /// A [`BufRead`] that never ends, to prove the cap does not read to EOF.
    static ENDLESS_CHUNK: [u8; 4096] = [b'x'; 4096];

    struct EndlessLine {
        served: usize,
    }

    impl io::Read for EndlessLine {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl BufRead for EndlessLine {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.served += ENDLESS_CHUNK.len();
            Ok(&ENDLESS_CHUNK)
        }

        fn consume(&mut self, _amount: usize) {}
    }
}
