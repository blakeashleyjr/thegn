//! Stub API for builds that omit the optional iroh call-home transport (THE-672).
//!
//! Keeping the same public seam lets provider and pane code compile unchanged;
//! operations that could start or use the transport report why it is absent.

use std::convert::Infallible;
use std::sync::Arc;

use thegn_core::iroh_wire::Hello;

use crate::provider::{ExecSession, ExecSpec};

/// Whether this build includes the iroh call-home transport.
pub const BUILT: bool = false;

/// Authorizes an incoming sandbox connection.
pub trait TokenVerifier: Send + Sync + 'static {
    fn verify(&self, hello: &Hello) -> Option<String>;
}

/// A [`TokenVerifier`] backed by a simple closure.
pub struct FnVerifier<F>(pub F);

impl<F> TokenVerifier for FnVerifier<F>
where
    F: Fn(&Hello) -> Option<String> + Send + Sync + 'static,
{
    fn verify(&self, hello: &Hello) -> Option<String> {
        (self.0)(hello)
    }
}

/// Uninhabited placeholder preserving the featured service API.
pub struct IrohHome {
    never: Infallible,
}

impl IrohHome {
    /// Report that the call-home transport was omitted from this build.
    pub async fn bind(
        _secret: Option<[u8; 32]>,
        _verifier: Arc<dyn TokenVerifier>,
    ) -> anyhow::Result<(Self, tokio::sync::mpsc::UnboundedReceiver<String>)> {
        Err(not_built())
    }

    /// An unbuilt home cannot have connections.
    pub fn is_connected(&self, _sandbox: &str) -> bool {
        match self.never {}
    }

    /// Report that the call-home transport was omitted from this build.
    pub async fn open_exec(&self, _sandbox: &str, _spec: ExecSpec) -> anyhow::Result<ExecSession> {
        match self.never {}
    }
}

/// Derive an endpoint id only in builds that include the iroh transport.
pub fn endpoint_id_for_secret(_secret: &[u8; 32]) -> anyhow::Result<String> {
    Err(not_built())
}

fn not_built() -> anyhow::Error {
    anyhow::anyhow!(
        "thegn was built without the `iroh` feature: the iroh call-home reach is not available in this binary"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{BUILT, FnVerifier, IrohHome};
    use thegn_core::iroh_wire::Hello;

    #[tokio::test]
    async fn stub_bind_reports_not_built() {
        let verifier = Arc::new(FnVerifier(|_: &Hello| None));
        let result = IrohHome::bind(None, verifier).await;
        let error = match result {
            Ok(_) => panic!("stub bind unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("built without the `iroh` feature")
        );
    }

    // The stub must report itself as not built; checked at compile time.
    const _: () = assert!(!BUILT);
}
