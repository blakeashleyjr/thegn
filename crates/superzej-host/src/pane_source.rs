//! The exec-session source seam: where a `Stream` pane's bytes come from.
//!
//! `pane.rs`'s relay (`relay_exec`) bridges an [`ExecSession`] to the shared
//! `PaneEvent` channel and owns the reconnect ladder (reattach the same
//! session → reopen fresh via the fallback spec). This trait abstracts *what*
//! it reconnects to, so the same relay serves:
//!   - [`ProviderSource`] — a managed-sandbox provider's native exec API
//!     (Sprites/Daytona/Fly/iroh call-home), the original hard-coded case, and
//!   - the pane daemon's client (control-plane warm-reattach), which slots in
//!     as another impl without touching the relay or the event loop.

use anyhow::Result;
use futures::future::BoxFuture;

use superzej_svc::provider::{ExecSession, ExecSpec, Provider};

/// A source of interactive exec sessions for one sandbox/daemon target.
/// Captures its target identity (sandbox id, socket, …) at construction so the
/// relay stays transport-blind.
pub(crate) trait ExecSource: Send + Sync {
    /// Open a fresh exec (the login shell etc.).
    fn open<'a>(&'a self, spec: &'a ExecSpec) -> BoxFuture<'a, Result<ExecSession>>;
    /// Reattach to a persisted session id (the server replays scrollback or a
    /// screen snapshot).
    fn attach<'a>(
        &'a self,
        session: &'a str,
        cols: u16,
        rows: u16,
    ) -> BoxFuture<'a, Result<ExecSession>>;
    /// Health feedback after an open/attach outcome (e.g. flips `exec=auto`
    /// panes to the CLI fallback during a cooldown). Default: no-op.
    fn report_health(&self, _ok: bool) {}
}

/// The managed-sandbox case: a [`Provider`] + sandbox id.
pub(crate) struct ProviderSource {
    pub provider: Provider,
    pub provider_name: String,
    pub sandbox_id: String,
}

impl ProviderSource {
    /// If this sandbox has dialed home over the iroh call-home reach, carry its
    /// interactive exec over iroh instead of the underlying provider's
    /// transport. Exec-only — lifecycle/fs still go through the real provider.
    /// Resolved per call (not once at relay start) so a reconnect picks up a
    /// sandbox that dialed home in the meantime; `Provider::Iroh` holds the
    /// home (not a specific connection), so it survives reconnects either way.
    fn iroh_override(&self) -> Option<Provider> {
        match crate::iroh_home::current() {
            Some(home) if home.is_connected(&self.sandbox_id) => Some(Provider::Iroh {
                home,
                sandbox: self.sandbox_id.clone(),
            }),
            _ => None,
        }
    }
}

impl ExecSource for ProviderSource {
    fn open<'a>(&'a self, spec: &'a ExecSpec) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(async move {
            match self.iroh_override() {
                Some(iroh) => iroh.open_exec(&self.sandbox_id, spec).await,
                None => self.provider.open_exec(&self.sandbox_id, spec).await,
            }
        })
    }

    fn attach<'a>(
        &'a self,
        session: &'a str,
        cols: u16,
        rows: u16,
    ) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(async move {
            match self.iroh_override() {
                Some(iroh) => {
                    iroh.attach_exec(&self.sandbox_id, session, cols, rows)
                        .await
                }
                None => {
                    self.provider
                        .attach_exec(&self.sandbox_id, session, cols, rows)
                        .await
                }
            }
        })
    }

    fn report_health(&self, ok: bool) {
        crate::agent::native_exec_report(&self.provider_name, ok);
    }
}
