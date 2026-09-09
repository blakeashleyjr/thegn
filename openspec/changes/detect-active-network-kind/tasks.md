# Tasks

- [ ] 1. Define pure network-medium selection for Ethernet, Wi-Fi, and unknown:
     physical default route first, sole active physical interface second, generic
     for VPN/tunnel/other default routes or remaining ambiguity.
- [ ] 2. Collect bounded active-interface/default-route facts off-loop on Linux
     and macOS with fixture-driven parsers; add a Windows provider or explicit
     unsupported/unknown result and document name-prefix fallback limits.
- [ ] 3. Make the built-in network icon mode automatic while preserving a
     literal `[stats].net_icon` as a fixed override; add configurable Wi-Fi,
     Ethernet, and generic glyphs with ASCII-safe fallbacks and reload coverage.
- [ ] 4. Project transport into statusbar rendering without changing throughput
     aggregation.
- [ ] 5. Add classifier fixtures for Ethernet-only, Wi-Fi-only, both with each
     default route, VPN default route, ambiguous links, loopback-only, quiet
     default route, and unknown medium.
- [ ] 6. Add statusbar coverage for automatic Ethernet/Wi-Fi/generic results,
     fixed override, zero traffic, aggregate-rate preservation, narrow width,
     supported themes, and Nerd Font/ASCII capabilities.
- [ ] 7. Add doctor selection/fallback evidence, update config/help, enforce the
     metrics idle-cost budget, and run focused metrics/render/OpenSpec gates.
