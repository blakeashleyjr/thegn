# Render network transport from active interface semantics

Linear: THE-92

## Why

The statusbar has one fixed Wi-Fi-shaped glyph for aggregate network traffic.
It cannot distinguish Ethernet, Wi-Fi, VPN-only, mixed, or unknown connectivity,
so the icon can make a false claim about the active transport.

## What Changes

- Extend the metrics snapshot with bounded, platform-derived active-interface
  and default-route facts without blocking the render loop.
- Select Ethernet or Wi-Fi only from a known physical default route, or from a
  sole active physical interface when route data is absent; VPN/tunnel,
  ambiguous, unsupported, and unknown observations use a generic icon.
- Preserve aggregate non-loopback throughput and literal `net_icon` overrides,
  while making the built-in default automatic with configurable semantic and
  ASCII-safe glyphs.
- Add Linux/macOS fixtures, explicit Windows/unsupported behavior, and doctor
  output explaining the selected interface/medium or generic fallback.

## Non-goals

- Network management, route mutation, or connectivity probing from the UI.
- Treating traffic volume alone as default-route authority.
- Replacing aggregate RX/TX with per-default-interface traffic.
- Guessing medium solely from an interface-name prefix when stronger platform
  facts are available.
