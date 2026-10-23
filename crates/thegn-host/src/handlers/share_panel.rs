//! Share-panel entry into the existing worktree-port share flow.

use crate::menu::{InputOverlay, MenuOverlay};
use thegn_core::config::Config;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShareEnter {
    Copy(String),
    Status(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShareOpen {
    Open(String),
    Status(String),
}

pub(crate) fn share_enter(shares: &[crate::share::ShareView], cursor: usize) -> Option<ShareEnter> {
    let view = shares.get(cursor)?;
    Some(match view.copy_value() {
        Some(value) => ShareEnter::Copy(value),
        None => ShareEnter::Status(not_ready_status(view)),
    })
}

pub(crate) fn share_open(shares: &[crate::share::ShareView], cursor: usize) -> ShareOpen {
    let Some(view) = shares.get(cursor) else {
        return ShareOpen::Status("No share URL to open".into());
    };
    if let Some(url) = view.browser_url() {
        return ShareOpen::Open(url.to_owned());
    }
    if view.copy_value().is_some() {
        return ShareOpen::Status(format!(
            "Share :{} has no browser link; ↵ copies its endpoint",
            view.port
        ));
    }
    ShareOpen::Status(not_ready_status(view))
}

fn not_ready_status(view: &crate::share::ShareView) -> String {
    if view.stopping {
        format!("Share :{} is stopping", view.port)
    } else if view.failed {
        format!(
            "Share :{} failed: {} — `x` clears it, `n` retries",
            view.port,
            view.error.as_deref().unwrap_or("unknown error")
        )
    } else {
        format!("Share :{} is still starting…", view.port)
    }
}

pub(crate) fn begin_share_port(
    cfg: &Config,
) -> (
    Option<MenuOverlay>,
    Option<(InputOverlay, crate::run::HostInputKind)>,
    String,
) {
    let reaches = cfg.share.configured_reaches();
    if reaches.len() >= 2 {
        (
            Some(crate::menu::reach_picker(&cfg.share)),
            None,
            "Share to… pick a reach (Esc cancels)".into(),
        )
    } else {
        let reach = reaches.first().copied();
        (
            None,
            Some((
                InputOverlay::new("share worktree port (number)", ""),
                crate::run::HostInputKind::ShareWorktreePort { reach },
            )),
            "Share port: enter a port number (Esc cancels)".into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::ShareProviderKind;

    fn endpoint_view(endpoint: thegn_svc::share::PublicEndpoint) -> crate::share::ShareView {
        crate::share::ShareView {
            worktree: "/wt".into(),
            port: 3000,
            url: Some(endpoint.canonical()),
            endpoint: Some(endpoint),
            failed: false,
            stopping: false,
            error: None,
            provider: "bore",
            public: true,
        }
    }

    fn ticket_view() -> crate::share::ShareView {
        endpoint_view(
            thegn_svc::share::PublicEndpoint::parse_ticket("opaque-ticket_7")
                .expect("valid opaque ticket"),
        )
    }

    fn socket_view() -> crate::share::ShareView {
        endpoint_view(
            thegn_svc::share::UrlRule::Fixed("bore.pub:4123".into())
                .fixed()
                .expect("valid socket endpoint"),
        )
    }

    #[test]
    fn share_enter_copies_ticket_as_data() {
        assert_eq!(
            share_enter(&[ticket_view()], 0),
            Some(ShareEnter::Copy("opaque-ticket_7".into()))
        );
    }

    #[test]
    fn share_enter_copies_canonical_url() {
        let view = endpoint_view(
            thegn_svc::share::PublicEndpoint::parse_url("https://Example.com:443")
                .expect("valid URL endpoint"),
        );
        assert_eq!(
            share_enter(&[view], 0),
            Some(ShareEnter::Copy("https://example.com".into()))
        );
    }

    #[test]
    fn share_enter_without_row() {
        assert_eq!(share_enter(&[], 0), None);
    }

    #[test]
    fn share_enter_reports_not_ready_rows() {
        let stopping = crate::share::ShareView {
            port: 3001,
            stopping: true,
            ..Default::default()
        };
        let failed = crate::share::ShareView {
            port: 3002,
            failed: true,
            error: Some("offline".into()),
            ..Default::default()
        };
        let starting = crate::share::ShareView {
            port: 3003,
            ..Default::default()
        };
        assert_eq!(
            share_enter(&[stopping], 0),
            Some(ShareEnter::Status("Share :3001 is stopping".into()))
        );
        assert_eq!(
            share_enter(&[failed], 0),
            Some(ShareEnter::Status(
                "Share :3002 failed: offline — `x` clears it, `n` retries".into()
            ))
        );
        assert_eq!(
            share_enter(&[starting], 0),
            Some(ShareEnter::Status("Share :3003 is still starting…".into()))
        );
    }

    #[test]
    fn share_open_opens_only_url_endpoints() {
        let url = endpoint_view(
            thegn_svc::share::PublicEndpoint::parse_url("https://Example.com:443")
                .expect("valid URL endpoint"),
        );
        assert_eq!(
            share_open(&[url], 0),
            ShareOpen::Open("https://example.com".into())
        );
        assert!(matches!(
            share_open(&[ticket_view()], 0),
            ShareOpen::Status(_)
        ));
        assert!(matches!(
            share_open(&[socket_view()], 0),
            ShareOpen::Status(_)
        ));
    }

    #[test]
    fn share_open_explains_ticket_and_socket_shares() {
        assert_eq!(
            share_open(&[ticket_view()], 0),
            ShareOpen::Status("Share :3000 has no browser link; ↵ copies its endpoint".into())
        );
        assert_eq!(
            share_open(&[socket_view()], 0),
            ShareOpen::Status("Share :3000 has no browser link; ↵ copies its endpoint".into())
        );
    }

    #[test]
    fn share_open_without_row() {
        assert_eq!(
            share_open(&[], 0),
            ShareOpen::Status("No share URL to open".into())
        );
    }

    #[test]
    fn begin_share_port_matches_reach_count() {
        for (cfg, expect_picker) in [
            (Config::default(), false),
            (
                Config {
                    share: thegn_core::config::ShareConfig {
                        provider: ShareProviderKind::Bore,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                false,
            ),
            (
                Config {
                    share: thegn_core::config::ShareConfig {
                        public: ShareProviderKind::Bore,
                        team: ShareProviderKind::Frp,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                true,
            ),
        ] {
            let (menu, input, _) = begin_share_port(&cfg);
            assert_eq!(menu.is_some(), expect_picker);
            assert_eq!(input.is_some(), !expect_picker);
        }
    }
}
