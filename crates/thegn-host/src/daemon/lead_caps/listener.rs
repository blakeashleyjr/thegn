//! The Lead's dedicated loopback control listener (THE-2171, THE-2188): bearer
//! tokens only (`local_admin = false`), started only when `[lead]` is
//! configured. With `[lead]` absent nothing runs: no listener, thread, timer or
//! spawn. With it present, only a blocking loopback `accept` exists (not a wake
//! source) until a Lead connects.
//!
//! The bound address is published to `$XDG_STATE_HOME/thegn/lead-control/
//! control-addr.json` (owner-only), where `lead::mcp::prepare` reads it when it
//! mints the Lead's scoped token.

/// The idle-invariant guard: the listener exists only with `[lead]` configured.
fn should_spawn(cfg: &thegn_core::config::Config) -> bool {
    cfg.lead.is_some()
}

/// Spawn the Lead listener when `[lead]` is configured. Must run inside the
/// daemon's tokio runtime. A bind or publish failure is logged and leaves the
/// Lead without tools (`prepare` then refuses with a restart hint); it never
/// fails the daemon.
pub(crate) fn spawn(
    cfg: &thegn_core::config::Config,
    state: thegn_svc::control::http::ControlState,
) {
    if !should_spawn(cfg) {
        return;
    }
    // The Lead runs an agent with a shell tool: implicit admin here would make
    // every scope limit meaningless.
    assert!(
        !state.local_admin,
        "the Lead listener must never grant implicit local admin"
    );
    let mut state = state;
    state.require_approval = true; // a code redeemed via the Lead listener always parks for operator approval
    let bound = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
        .and_then(|l| l.local_addr().map(|addr| (l, addr)));
    let (std_listener, addr) = match bound {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!(target: "thegn::daemon", %error, "Lead listener: bind 127.0.0.1:0 failed");
            return;
        }
    };
    let listener = match tokio::net::TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(error) => {
            tracing::warn!(target: "thegn::daemon", %error, "Lead listener: adopt socket failed");
            return;
        }
    };
    if let Err(error) =
        crate::lead::mcp::write_control_addr(&thegn_core::util::xdg_state_home(), addr)
    {
        tracing::warn!(target: "thegn::daemon", error = %format!("{error:#}"), "Lead listener: publish address failed");
        return;
    }
    let app = thegn_svc::control::http::router(state);
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!(target: "thegn::daemon", %error, "Lead control listener exited");
        }
    });
    tracing::info!(target: "thegn::daemon", %addr, "Lead control listener (bearer tokens only)");
}

#[cfg(test)]
mod tests {
    use super::should_spawn;

    #[test]
    fn a_disabled_lead_spawns_nothing() {
        let mut cfg = thegn_core::config::Config::default();
        assert!(cfg.lead.is_none());
        assert!(!should_spawn(&cfg));
        cfg.lead = Some(thegn_core::config_lead::LeadConfig::default());
        assert!(should_spawn(&cfg));
    }
}
