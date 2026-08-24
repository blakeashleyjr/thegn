//! The provider-as-plugin registry: the loop-owned plugin state publishes
//! its live issue-provider bridges here, and the hydration workers (which
//! build an `IssueRouter` from config on their own threads) append them.
//!
//! Process-global by the same argument as `ci_refresh`'s health map: the
//! producers (the plugins drain) and consumers (hydration, `cmd/kaneo`-style
//! CLI paths) share no other state, and the registry is a snapshot — a dead
//! plugin's bridge errors on send, and the drain removes it on `Exit`.

use std::sync::{Arc, Mutex, OnceLock};

use thegn_svc::issue::IssueBackend;
use thegn_svc::plugin::{PluginIssueBackend, ProviderBridge};

/// One live plugin issue provider: `(plugin id, account label, bridge)`.
type Row = (String, String, Arc<ProviderBridge>);

fn registry() -> &'static Mutex<Vec<Row>> {
    static REG: OnceLock<Mutex<Vec<Row>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Replace the published set (the drain calls this after Loaded/Exit/reload).
pub(crate) fn set_issue_providers(rows: Vec<Row>) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    *reg = rows;
}

/// Fresh backends over the live bridges, one per registered provider. Each
/// call constructs new adapters (they are thin: an `Arc` clone + a leaked
/// id), so a router built on any thread gets the current set.
pub(crate) fn issue_backends() -> Vec<(String, Box<dyn IssueBackend>)> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.iter()
        .map(|(plugin, account, bridge)| {
            (
                account.clone(),
                Box::new(PluginIssueBackend::new(bridge.clone(), plugin)) as Box<dyn IssueBackend>,
            )
        })
        .collect()
}

/// Append every registered plugin issue provider to `router`.
pub(crate) fn extend_issue_router(router: &mut thegn_svc::issue::IssueRouter) {
    for (account, backend) in issue_backends() {
        router.push_backend(account, backend);
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
        set_issue_providers(vec![("demo".into(), "Demo".into(), bridge)]);
        let backends = issue_backends();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].0, "Demo");
        assert_eq!(backends[0].1.provider_id(), "plugin:demo");
        set_issue_providers(Vec::new());
        assert!(issue_backends().is_empty());
    }

    fn fake_session() -> (thegn_svc::plugin::SessionWriter, ()) {
        // Spawn a trivial child purely to obtain a writer; close it at once.
        let s = thegn_svc::plugin::ResidentSession::spawn(
            &["sh".into(), "-c".into(), "cat >/dev/null".into()],
            &Default::default(),
            None,
            |_| {},
        )
        .unwrap();
        let w = s.writer();
        s.kill();
        (w, ())
    }
}
