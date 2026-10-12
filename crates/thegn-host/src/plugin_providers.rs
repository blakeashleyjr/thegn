//! The provider-as-plugin registry: the loop-owned plugin state publishes
//! its live issue-provider bridges here, and the hydration workers (which
//! build an `IssueRouter` from config on their own threads) append them.
//!
//! Process-global by the same argument as `ci_refresh`'s health map: the
//! producers (the plugins drain) and consumers (hydration, `cmd/kaneo`-style
//! CLI paths) share no other state, and the registry is a snapshot — a dead
//! plugin's bridge errors on send, and the drain removes it on `Exit`.

use std::sync::{Arc, Mutex, OnceLock};

use thegn_core::issue::{Issue, IssueDetail, IssueDraft, IssueFilter, IssuePatch};
use thegn_svc::issue::{IssueBackend, IssueCaps, IssueError};
use thegn_svc::plugin::{PluginIssueBackend, ProviderBridge};

/// One live plugin issue provider: `(plugin id, account label, bridge, caps)`.
type Row = (String, String, Arc<ProviderBridge>, IssueCaps);

fn registry() -> &'static Mutex<Vec<Row>> {
    static REG: OnceLock<Mutex<Vec<Row>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Guard a router-held adapter against use after its bridge is removed from
/// the published provider set.
struct LiveBackend {
    inner: PluginIssueBackend,
    bridge: Arc<ProviderBridge>,
}

impl LiveBackend {
    fn ensure_live(&self) -> Result<(), IssueError> {
        let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if reg.iter().any(|row| Arc::ptr_eq(&row.2, &self.bridge)) {
            Ok(())
        } else {
            Err(IssueError::Api(
                "plugin issue provider handle is stale: the plugin was reloaded, restarted or removed"
                    .into(),
            ))
        }
    }
}

impl IssueBackend for LiveBackend {
    fn provider_id(&self) -> &str {
        self.inner.provider_id()
    }

    fn caps(&self) -> IssueCaps {
        self.inner.caps()
    }

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> futures::future::BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.list_issues(filter).await
        })
    }

    fn get_issue<'a>(
        &'a self,
        id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.get_issue(id).await
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> futures::future::BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.create_issue(draft).await
        })
    }

    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> futures::future::BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.update_issue(id, patch).await
        })
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> futures::future::BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.search(query, limit).await
        })
    }

    fn add_comment<'a>(
        &'a self,
        id: &'a str,
        body: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.add_comment(id, body).await
        })
    }

    fn attach_label<'a>(
        &'a self,
        id: &'a str,
        label: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.attach_label(id, label).await
        })
    }

    fn detach_label<'a>(
        &'a self,
        id: &'a str,
        label: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            self.ensure_live()?;
            self.inner.detach_label(id, label).await
        })
    }
}

/// Replace the published set (the drain calls this after Loaded/Exit/reload).
pub(crate) fn set_issue_providers(rows: Vec<Row>) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    *reg = rows;
}

/// Fresh backends over the live bridges, one per registered provider. Each
/// call constructs new adapters (they are thin: an `Arc` clone + an owned
/// immutable id), so a router built on any thread gets the current set and
/// dropping an old router releases its generation's identity.
pub(crate) fn issue_backends() -> Vec<(String, Box<dyn IssueBackend>)> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.iter()
        .filter_map(|(plugin, account, bridge, caps)| {
            let backend = match PluginIssueBackend::new(bridge.clone(), plugin, *caps) {
                Ok(backend) => backend,
                Err(error) => {
                    tracing::warn!(plugin, error = %error, "rejecting invalid plugin issue namespace");
                    return None;
                }
            };
            Some((
                account.clone(),
                Box::new(LiveBackend {
                    inner: backend,
                    bridge: bridge.clone(),
                }) as Box<dyn IssueBackend>,
            ))
        })
        .collect()
}

/// Append every registered plugin issue provider to `router`.
pub(crate) fn extend_issue_router(router: &mut thegn_svc::issue::IssueRouter) {
    for (account, backend) in issue_backends() {
        if let Err(error) = router.push_backend(account, backend) {
            tracing::warn!(error = %error, "rejecting invalid issue provider registration");
        }
    }
}

#[cfg(test)]
mod tests {
    // The registry is process-global, and the suite runs tests in parallel —
    // exercise set/get in ONE test so nothing races the shared slot.
    use super::*;

    #[test]
    fn registry_round_trips_and_replaces() {
        set_issue_providers(Vec::new());
        assert!(issue_backends().is_empty());
        // A bridge over a closed writer still registers; calls just error.
        let (session, _rx) = fake_session();
        let bridge = ProviderBridge::new(session, std::time::Duration::from_millis(50));
        let caps = IssueCaps {
            comments: true,
            labels: false,
            ..Default::default()
        };
        set_issue_providers(vec![("demo".into(), "Demo".into(), bridge, caps)]);
        let backends = issue_backends();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].0, "Demo");
        assert_eq!(backends[0].1.provider_id(), "plugin:demo");
        assert_eq!(backends[0].1.caps(), caps);
        set_issue_providers(Vec::new());
        assert!(issue_backends().is_empty());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(backends[0].1.get_issue("plugin:demo:1"))
            .unwrap_err();
        assert!(matches!(
            error,
            IssueError::Api(message)
                if message.contains("plugin issue provider handle is stale")
        ));
    }

    fn fake_session() -> (thegn_svc::plugin::SessionWriter, ()) {
        // Spawn a trivial child purely to obtain a writer; close it at once.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let supervisor = thegn_svc::plugin::ResidentSupervisor::new(runtime.handle().clone());
        let s = thegn_svc::plugin::ResidentSession::spawn(
            &supervisor,
            "fixture",
            &["sh".into(), "-c".into(), "cat >/dev/null".into()],
            &Default::default(),
            None,
            |_| {},
        )
        .unwrap();
        let w = s.writer();
        s.kill();
        assert!(
            runtime
                .block_on(supervisor.shutdown_until(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3)
                ))
                .is_settled()
        );
        (w, ())
    }
}
