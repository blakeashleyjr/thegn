//! Provider-seam glue: the tokio-aware half of [`thegn_core::seam`].
//!
//! `thegn_core::seam` holds the pure vocabulary (error classes, probes, the
//! `Kind` trait every `config_enum!` kind implements). This module holds what
//! needs a runtime:
//!
//! - [`blocking`] — wrap a blocking (subprocess) body as the [`BoxFuture`] an
//!   object-safe seam method returns.
//! - [`Ladder`] — ordered degradation (native → CLI → unavailable): the first
//!   layer whose answer is not a "this layer can't" error wins.
//! - [`Router`] — multi-account fan-out with per-account failure isolation
//!   and `"<provider>:<key>"` id routing (the `IssueRouter` idiom, generic).
//! - [`registry`] — build every provider the loaded config selects and
//!   collect their [`ProbeReport`]s for `thegn doctor`.
//! - [`kind_coverage`] — the test helper that pins "a kind is implemented or
//!   reserved" for one seam's factory.
//!
//! Nothing here is a seam itself; each seam gets a one-line
//! `impl T for Ladder<dyn T>` / `Router<dyn T>` forwarding block when it
//! migrates onto the pattern.

pub mod ladder;
pub mod registry;
pub mod router;

pub use ladder::Ladder;
pub use router::Router;
pub use thegn_core::seam::{
    Availability, BoxFuture, ErrorClass, Kind, Probe, ProbeReport, SeamError,
};

/// Run a blocking body (typically a subprocess) as a seam future. The body
/// runs on tokio's blocking pool, so a CLI-backed provider never stalls the
/// runtime's worker threads — the same discipline the daemon's
/// `spawn_blocking` git calls follow.
pub fn blocking<'a, T, E, F>(f: F) -> BoxFuture<'a, Result<T, E>>
where
    T: Send + 'static,
    E: SeamError + From<JoinFailure>,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    Box::pin(async move {
        match tokio::task::spawn_blocking(f).await {
            Ok(r) => r,
            Err(join) => Err(E::from(JoinFailure(join.to_string()))),
        }
    })
}

/// The blocking pool refused or lost the task (panic / runtime shutdown).
/// Seam errors convert it to their `Other` class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinFailure(pub String);

impl std::fmt::Display for JoinFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blocking task failed: {}", self.0)
    }
}

/// Assert that a seam's factory agrees with its kind's `reserved` markers:
/// `factory(k)` is `Some` exactly when `k` is not reserved. One call per seam
/// in that seam's tests pins the "implemented or reserved" rule.
pub fn kind_coverage<K: Kind + std::fmt::Debug, T>(factory: impl Fn(K) -> Option<T>) {
    for k in K::ALL {
        let built = factory(*k).is_some();
        assert_eq!(
            built,
            !k.is_reserved(),
            "kind {:?} ({}) is {} but the factory returned {}",
            k,
            k.as_str(),
            if k.is_reserved() {
                "reserved"
            } else {
                "implemented"
            },
            if built { "Some" } else { "None" },
        );
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
        fn unsupported(_: &'static str) -> Self {
            E(ErrorClass::Unsupported)
        }
    }
    impl From<JoinFailure> for E {
        fn from(_: JoinFailure) -> Self {
            E(ErrorClass::Other)
        }
    }

    #[tokio::test]
    async fn blocking_runs_body_and_maps_panics_to_other() {
        let ok: Result<u8, E> = blocking(|| Ok(7)).await;
        assert_eq!(ok.unwrap(), 7);
        let err: Result<u8, E> = blocking(|| Err(E(ErrorClass::Auth))).await;
        assert_eq!(err.unwrap_err().class(), ErrorClass::Auth);
        let boom: Result<u8, E> = blocking(|| -> Result<u8, E> { panic!("x") }).await;
        assert_eq!(boom.unwrap_err().class(), ErrorClass::Other);
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum K {
        A,
        R,
    }
    impl Kind for K {
        const ALL: &'static [K] = &[K::A, K::R];
        fn as_str(self) -> &'static str {
            match self {
                K::A => "a",
                K::R => "r",
            }
        }
        fn is_reserved(self) -> bool {
            self == K::R
        }
    }

    #[test]
    fn kind_coverage_accepts_an_agreeing_factory() {
        kind_coverage(|k: K| (k == K::A).then_some(()));
    }

    #[test]
    #[should_panic(expected = "is reserved but the factory returned Some")]
    fn kind_coverage_rejects_building_a_reserved_kind() {
        kind_coverage(|_: K| Some(()));
    }

    #[test]
    #[should_panic(expected = "is implemented but the factory returned None")]
    fn kind_coverage_rejects_a_missing_impl() {
        kind_coverage(|_: K| None::<()>);
    }
}
