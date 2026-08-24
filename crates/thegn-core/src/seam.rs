//! The provider-seam vocabulary.
//!
//! Every substitutable backend in thegn (forge, CI, issue tracker, calendar,
//! media, git, sandbox, editor, remote provider, …) is a *seam*: a trait with
//! one or more implementations selected by a config `kind`. This module is
//! the pure, substrate-free part of that pattern — the types the `config_enum!`
//! macro, `thegn doctor`, and every seam's error type agree on. The
//! tokio-aware glue (degradation ladders, multi-account routers, the probe
//! registry) lives in `thegn_svc::seam`.
//!
//! Rules the rest of the workspace converges on:
//!
//! - A seam trait is **object-safe**: async methods return [`BoxFuture`], so a
//!   router is `Vec<Box<dyn T>>` and never a hand-written delegation enum.
//! - An optional operation exists iff it has a caps bit, and its default body
//!   returns `E::unsupported(op)` — see [`SeamError`].
//! - A config `kind` value is either implemented or `reserved` — see [`Kind`].
//! - Every implementation can describe itself — see [`Probe`].

use serde::{Deserialize, Serialize};

/// The future type object-safe seam traits return. Deliberately not a
/// `futures` dependency: a `Pin<Box<dyn Future>>` alias needs only std.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Uniform classification every seam error exposes. It drives three things:
/// degradation ladders (which errors fall through to the next layer), the
/// connectivity holder (which errors mean "offline" rather than "bad token"),
/// and `thegn doctor` (what to print).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// The provider declares it cannot do this operation (caps bit off).
    Unsupported,
    /// A binary / native client this layer needs is absent.
    NotInstalled,
    /// No account / token / URL is configured for this layer.
    NotConfigured,
    /// Credentials were present but rejected.
    Auth,
    /// Connect / timeout / 5xx — retrying later may succeed.
    Transient,
    /// The upstream object does not exist.
    NotFound,
    /// The upstream asked us to slow down.
    RateLimited,
    /// Anything else.
    Other,
}

impl ErrorClass {
    /// Whether a degradation ladder should try the next layer after an error
    /// of this class. Only "this layer can't" classes fall through; a real
    /// answer from upstream (auth, not-found, rate-limit, transient) is final
    /// — retrying it on a worse layer would just repeat the failure with a
    /// less specific message.
    pub fn falls_through(self) -> bool {
        matches!(
            self,
            ErrorClass::Unsupported | ErrorClass::NotInstalled | ErrorClass::NotConfigured
        )
    }
}

/// The error contract a seam's error type implements.
pub trait SeamError: std::error::Error + Send + Sync + 'static {
    /// Classify for ladders / connectivity / doctor.
    fn class(&self) -> ErrorClass;
    /// The value a defaulted optional operation returns. `op` is the method
    /// name, so the message a user sees names what the provider lacks.
    fn unsupported(op: &'static str) -> Self;
    /// Transient connectivity failure (feeds the global connectivity holder).
    fn is_transient(&self) -> bool {
        self.class() == ErrorClass::Transient
    }
    /// Whether a ladder should fall through past this error.
    fn falls_through(&self) -> bool {
        self.class().falls_through()
    }
}

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum Availability {
    /// Fully usable.
    Ready,
    /// Usable, but a better layer is missing (e.g. native client absent,
    /// CLI fallback in use).
    Degraded(String),
    /// Not usable; the reason names what is missing (binary, token, reserved
    /// kind, …).
    Unavailable(String),
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready)
    }
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Availability::Unavailable(_))
    }
}

/// One provider's self-description, as printed by `thegn doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProbeReport {
    /// The seam: `"forge"`, `"ci"`, `"issues"`, `"git"`, `"sandbox"`, …
    pub seam: String,
    /// The provider id within the seam: `"github"`, `"gix"`, `"podman"`,
    /// `"linear:work"` (per-account seams suffix the account name).
    pub id: String,
    pub availability: Availability,
    /// The seam's caps struct, serialized. `Value::Null` when a seam has no
    /// optional operations.
    #[serde(default)]
    pub caps: serde_json::Value,
    /// Free-form detail lines (`"native: octocrab"`, `"fallback: gh 2.63"`).
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ProbeReport {
    pub fn new(seam: &'static str, id: impl Into<String>, availability: Availability) -> Self {
        ProbeReport {
            seam: seam.to_string(),
            id: id.into(),
            availability,
            caps: serde_json::Value::Null,
            notes: Vec::new(),
        }
    }
    pub fn with_caps<C: Serialize>(mut self, caps: &C) -> Self {
        self.caps = serde_json::to_value(caps).unwrap_or(serde_json::Value::Null);
        self
    }
    pub fn note(mut self, line: impl Into<String>) -> Self {
        self.notes.push(line.into());
        self
    }
    /// A report for a selection that names a reserved (accepted but
    /// unimplemented) kind — the registry emits these so doctor can explain
    /// *why* a seam is unavailable.
    pub fn reserved(seam: &'static str, kind: &str) -> Self {
        ProbeReport::new(
            seam,
            kind,
            Availability::Unavailable(format!(
                "{kind} is reserved: accepted by config but not implemented in this build"
            )),
        )
    }
}

/// Every provider implementation describes itself. Probes are cheap and
/// synchronous by contract (a `--version`, a `which`, a config check — never a
/// network round-trip), because doctor runs them all in sequence.
pub trait Probe: Send + Sync {
    fn probe(&self) -> ProbeReport;
}

/// Implemented by every `config_enum!`-declared provider kind. `ALL` lets a
/// kind-coverage test walk the values; `is_reserved` is the macro-emitted
/// fact that a value is accepted by config but has no implementation.
pub trait Kind: Copy + 'static {
    const ALL: &'static [Self];
    fn as_str(self) -> &'static str;
    fn is_reserved(self) -> bool;
    /// The implemented (non-reserved) values.
    fn implemented() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied().filter(|k| !k.is_reserved())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct E(ErrorClass);
    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    impl std::error::Error for E {}
    impl SeamError for E {
        fn class(&self) -> ErrorClass {
            self.0
        }
        fn unsupported(_op: &'static str) -> Self {
            E(ErrorClass::Unsupported)
        }
    }

    #[test]
    fn only_this_layer_cant_classes_fall_through() {
        let through = [
            ErrorClass::Unsupported,
            ErrorClass::NotInstalled,
            ErrorClass::NotConfigured,
        ];
        let final_ = [
            ErrorClass::Auth,
            ErrorClass::Transient,
            ErrorClass::NotFound,
            ErrorClass::RateLimited,
            ErrorClass::Other,
        ];
        for c in through {
            assert!(c.falls_through(), "{c:?}");
            assert!(E(c).falls_through());
        }
        for c in final_ {
            assert!(!c.falls_through(), "{c:?}");
            assert!(!E(c).falls_through());
        }
    }

    #[test]
    fn seam_error_defaults() {
        assert!(E(ErrorClass::Transient).is_transient());
        assert!(!E(ErrorClass::Auth).is_transient());
        assert_eq!(E::unsupported("x").class(), ErrorClass::Unsupported);
    }

    #[test]
    fn availability_predicates_and_serde() {
        assert!(Availability::Ready.is_ready());
        assert!(!Availability::Ready.is_unavailable());
        let u = Availability::Unavailable("no gh".into());
        assert!(u.is_unavailable());
        assert!(!u.is_ready());
        let d = Availability::Degraded("cli fallback".into());
        assert!(!d.is_ready() && !d.is_unavailable());
        let j = serde_json::to_string(&u).unwrap();
        assert_eq!(j, r#"{"state":"unavailable","reason":"no gh"}"#);
        let back: Availability = serde_json::from_str(&j).unwrap();
        assert_eq!(back, u);
        assert_eq!(
            serde_json::to_string(&Availability::Ready).unwrap(),
            r#"{"state":"ready"}"#
        );
    }

    #[test]
    fn probe_report_builders_and_round_trip() {
        #[derive(Serialize)]
        struct Caps {
            pr: bool,
        }
        let r = ProbeReport::new("forge", "github", Availability::Ready)
            .with_caps(&Caps { pr: true })
            .note("native: octocrab");
        assert_eq!(r.caps, serde_json::json!({"pr": true}));
        assert_eq!(r.notes, vec!["native: octocrab"]);
        let j = serde_json::to_string(&r).unwrap();
        let back: ProbeReport = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
        // Missing optional fields default.
        let min: ProbeReport =
            serde_json::from_str(r#"{"seam":"ci","id":"gitlab","availability":{"state":"ready"}}"#)
                .unwrap();
        assert_eq!(min.caps, serde_json::Value::Null);
        assert!(min.notes.is_empty());
    }

    #[test]
    fn reserved_report_names_the_kind() {
        let r = ProbeReport::reserved("ci", "drone");
        assert_eq!(r.id, "drone");
        match &r.availability {
            Availability::Unavailable(reason) => {
                assert!(reason.contains("drone"));
                assert!(reason.contains("reserved"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_class_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ErrorClass::NotInstalled).unwrap(),
            r#""not_installed""#
        );
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum K {
        A,
        B,
        R,
    }
    impl Kind for K {
        const ALL: &'static [K] = &[K::A, K::B, K::R];
        fn as_str(self) -> &'static str {
            match self {
                K::A => "a",
                K::B => "b",
                K::R => "r",
            }
        }
        fn is_reserved(self) -> bool {
            self == K::R
        }
    }

    #[test]
    fn kind_implemented_filters_reserved() {
        let got: Vec<K> = K::implemented().collect();
        assert_eq!(got, vec![K::A, K::B]);
        assert_eq!(K::R.as_str(), "r");
    }
}
