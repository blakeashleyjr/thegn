//! The egress plan of one sandbox launch (resolutions B11).
//!
//! Stub landed by WS00-P1-03; owned by WS04-P2-08, which adds the builder
//! (`plan::build`, the only way consumers construct a plan), `route`,
//! `capability` and the `peer: PeerProof` field. Until then the types exist so
//! WS02, WS03, WS05, WS06, WS07, WS10 and WS11 compile against one set of names,
//! and [`SandboxSpec::egress`](crate::sandbox::SandboxSpec::egress) is always
//! `None`.
//!
//! Meanings (B11):
//! - an [`EgressPlan`] binds one sandbox to one authenticated proxy lease;
//! - [`ProxyAuth::Token`] is always present (B13), and its [`LeaseToken`] never
//!   prints: its `Debug` is redacted and it has no `Display`;
//! - [`EgressRoute`] is what a backend lowers: no route at all, the
//!   unmanaged direct route, or the proxy route of a plan.
//!
//! No code outside this module's tests may build an [`EgressPlan`] with a struct
//! literal: WS04-P2-08 adds a ninth field, and `plan::build` is the one
//! constructor.

use std::net::SocketAddr;
use std::path::PathBuf;

/// The datapath a plan uses to reach the egress proxy. Stub (WS00-P1-03);
/// owner WS04-P2-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EgressPath {
    /// A bridge into the sandbox's own network namespace (bwrap, systemd).
    NetnsBridge,
    /// A sidecar sharing the sandbox's network namespace (OCI, Linux-local).
    SidecarNetns,
    /// An engine-internal network relay; credited only after per-engine
    /// evidence.
    InternalRelay,
    /// A loopback port the sandbox may reach (Seatbelt).
    LoopbackPort,
    /// A kernel pin to the lease port on any address (Landlock-only TCP pin).
    PortPin,
    /// A Windows WFP proxy-only filter.
    WfpProxyOnly,
    /// The provider's own egress controls.
    ProviderNative,
    /// A user-configured external proxy.
    External,
}

/// Where the sandbox reaches the proxy. Stub (WS00-P1-03); owner WS04-P2-08.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyEndpoint {
    /// A unix-socket directory bound into the sandbox.
    UnixDir {
        /// The directory on the host.
        host_dir: PathBuf,
        /// Where it appears inside the sandbox.
        in_sandbox: PathBuf,
    },
    /// A loopback address the sandbox can connect to.
    Loopback {
        /// The proxy's listening address.
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

/// The secret half of a lease credential. Its `Debug` prints
/// `LeaseToken(***)` and it has no `Display`, so it cannot leak through logs or
/// error text. Stub (WS00-P1-03); owner WS04-P2-08.
#[derive(Clone, PartialEq, Eq)]
pub struct LeaseToken(String);

impl LeaseToken {
    /// Wrap a token value. WS04-P2-08 mints tokens; nothing does in WS00.
    pub fn new(token: String) -> Self {
        LeaseToken(token)
    }

    /// The token value, for the datapath that presents it to the proxy only.
    /// Never log or display the result.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for LeaseToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LeaseToken(***)")
    }
}

/// How the sandbox authenticates to the proxy; always present (B13). Stub
/// (WS00-P1-03); owner WS04-P2-08.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyAuth {
    /// Proxy credentials bound to one lease.
    Token {
        /// The proxy user name that names the lease.
        lease_user: String,
        /// The lease's secret.
        token: LeaseToken,
    },
}

/// The identity of one egress lease. Stub (WS00-P1-03); owner WS04-P2-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeaseId(pub [u8; 16]);

/// One sandbox's egress plan: the lease, the datapath, the endpoint, the
/// credentials and what the launch must add (env, binds). Stub (WS00-P1-03);
/// owner WS04-P2-08, which adds `peer` and the `plan::build` constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressPlan {
    /// The lease this plan is bound to.
    pub lease: LeaseId,
    /// The datapath to the proxy.
    pub path: EgressPath,
    /// Where the sandbox reaches the proxy.
    pub endpoint: ProxyEndpoint,
    /// How the sandbox authenticates.
    pub auth: ProxyAuth,
    /// Environment the launch sets inside the sandbox (proxy variables).
    pub env: Vec<(String, String)>,
    /// Extra binds the launch adds, as (host, in-sandbox) pairs.
    pub binds: Vec<(PathBuf, PathBuf)>,
    /// SHA-256 of the resolved egress policy the lease enforces.
    pub policy_digest: [u8; 32],
}

/// The route a backend lowers for one launch. Stub (WS00-P1-03); owner
/// WS04-P2-08, which adds `route(&SandboxSpec)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressRoute<'a> {
    /// No route out at all.
    Denied,
    /// The direct, unmanaged route.
    Unmanaged,
    /// Out through the proxy of this plan only.
    Proxy(&'a EgressPlan),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "lease-secret-5c1e";

    fn plan() -> EgressPlan {
        EgressPlan {
            lease: LeaseId([7; 16]),
            path: EgressPath::NetnsBridge,
            endpoint: ProxyEndpoint::UnixDir {
                host_dir: PathBuf::from("/run/thegn/egress/lease"),
                in_sandbox: PathBuf::from("/run/thegn-egress"),
            },
            auth: ProxyAuth::Token {
                lease_user: "lease-0707".into(),
                token: LeaseToken::new(SECRET.into()),
            },
            env: vec![("HTTPS_PROXY".into(), "http://127.0.0.1:3128".into())],
            binds: vec![(
                PathBuf::from("/run/thegn/egress/lease"),
                PathBuf::from("/run/thegn-egress"),
            )],
            policy_digest: [1; 32],
        }
    }

    #[test]
    fn lease_token_debug_redacts() {
        let token = LeaseToken::new(SECRET.into());
        assert_eq!(format!("{token:?}"), "LeaseToken(***)");
        assert_eq!(token.expose_secret(), SECRET);
        let rendered = format!("{:?}", plan());
        assert!(!rendered.contains(SECRET), "plan Debug leaked the token");
        assert!(rendered.contains("LeaseToken(***)"));
    }

    #[test]
    fn egress_plan_clone_eq_roundtrip() {
        let original = plan();
        let copy = original.clone();
        assert_eq!(original, copy);

        let other_endpoints = [
            ProxyEndpoint::Loopback {
                addr: SocketAddr::from(([127, 0, 0, 1], 3128)),
            },
            ProxyEndpoint::Relay {
                host: "relay.internal".into(),
                port: 3128,
            },
        ];
        for endpoint in other_endpoints {
            let changed = EgressPlan {
                endpoint,
                ..original.clone()
            };
            assert_ne!(changed, original);
            assert_eq!(changed.clone(), changed);
        }

        let rotated = EgressPlan {
            auth: ProxyAuth::Token {
                lease_user: "lease-0707".into(),
                token: LeaseToken::new("another".into()),
            },
            ..original.clone()
        };
        assert_ne!(rotated, original, "a different token is a different plan");

        let paths = [
            EgressPath::NetnsBridge,
            EgressPath::SidecarNetns,
            EgressPath::InternalRelay,
            EgressPath::LoopbackPort,
            EgressPath::PortPin,
            EgressPath::WfpProxyOnly,
            EgressPath::ProviderNative,
            EgressPath::External,
        ];
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len());
        assert!(format!("{:?}", LeaseId([0; 16])).starts_with("LeaseId("));
    }

    #[test]
    fn egress_route_borrows_the_plan() {
        let plan = plan();
        let route = EgressRoute::Proxy(&plan);
        let copy = route;
        assert_eq!(route, copy);
        assert_ne!(route, EgressRoute::Denied);
        assert_ne!(EgressRoute::Denied, EgressRoute::Unmanaged);
        match copy {
            EgressRoute::Proxy(inner) => assert_eq!(inner.path, EgressPath::NetnsBridge),
            EgressRoute::Denied | EgressRoute::Unmanaged => panic!("expected a proxy route"),
        }
    }
}
