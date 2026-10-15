//! Share-panel entry into the existing worktree-port share flow.

use crate::menu::{InputOverlay, MenuOverlay};
use thegn_core::config::Config;

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
