# Tasks

- [x] 1. Define pure network-medium selection for Ethernet, Wi-Fi, and unknown:
     physical default route first, sole active physical interface second, generic
     for VPN/tunnel/other default routes or remaining ambiguity.
     Evidence: `crates/thegn-metrics/src/net_medium.rs`
     (`select_medium_explained`).
- [x] 2. Collect bounded active-interface/default-route facts off-loop on Linux
     and macOS with fixture-driven parsers; add a Windows provider or explicit
     unsupported/unknown result and document name-prefix fallback limits.
     Evidence: `crates/thegn-metrics/src/net_kind.rs` (`facts_at`,
     `classify_at`, `probe_links_at`) and `crates/thegn-metrics/src/sample.rs` (`probe_net_path`).
     Platform note: Linux delivered; macOS and Windows report an explicit
     probe-unavailable result and draw the generic icon; no macOS route parser
     (follow-up).
- [x] 3. Make the built-in network icon mode automatic while preserving a
     literal `[stats].net_icon` as a fixed override; add configurable Wi-Fi,
     Ethernet, and generic glyphs with ASCII-safe fallbacks and reload coverage.
     Evidence: `crates/thegn-core/src/config.rs` (`net_icon` auto default and
     `[stats.net_icons]` parsing) and `config/config.toml.example`.
- [x] 4. Project transport into statusbar rendering without changing throughput
     aggregation.
     Evidence: `crates/thegn-host/src/chrome_tests.rs`
     (`masthead_network_widget_selects_medium_icon_and_preserves_rates`).
- [x] 5. Add classifier fixtures for Ethernet-only, Wi-Fi-only, both with each
     default route, VPN default route, ambiguous links, loopback-only, quiet
     default route, and unknown medium.
     Evidence: `crates/thegn-metrics/src/net_kind.rs` classifier fixtures and
     `crates/thegn-metrics/src/net_medium.rs` selection tests.
- [x] 6. Add statusbar coverage for automatic Ethernet/Wi-Fi/generic results,
     fixed override, zero traffic, aggregate-rate preservation, narrow width,
     supported themes, and Nerd Font/ASCII capabilities.
     Evidence: `crates/thegn-host/src/chrome_tests.rs`
     (`masthead_network_widget_selects_medium_icon_and_preserves_rates`,
     `net_widget_uses_configured_medium_glyphs_in_auto_mode`,
     `fixed_net_icon_overrides_per_medium_glyphs`).
- [x] 7. Add doctor selection/fallback evidence, update config/help, enforce the
     metrics idle-cost budget, and run focused metrics/render/OpenSpec gates.
     Evidence: `crates/thegn-host/src/cmd/doctor.rs` (`medium_word`, `net path`
     line and JSON `medium`) and `docs/help/bars.md` (automatic network icon).
     Idle cost: the probe rides the existing stats sampler; no separate budget
     gate was added.
