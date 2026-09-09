# Design

## Facts and classification

Sampling retains aggregate non-loopback RX/TX and adds a bounded fact set for
active interfaces: stable interface identity, up/connected state, physical
medium (`ethernet`, `wifi`, or `other/unknown`), and whether the system default
route selects that interface. Classification is pure and does not use current
byte volume as a connectivity test; a quiet default-route interface remains a
candidate.

Selection precedence is deliberately small:

1. A known Ethernet or Wi-Fi interface carrying the default route selects that
   medium, even if another physical interface is also up.
2. If no default route is observable, exactly one active known physical
   interface selects its medium.
3. A VPN/tunnel/other default route, multiple remaining physical candidates,
   no active physical interface, permission failure, or unsupported provider
   selects `unknown` and therefore the generic icon.

A VPN is not displayed as a separately inferred overlay in this change. When it
owns the default route and the underlying path cannot be established from
authoritative platform data, generic is the truthful result.

## Platform providers and cost

Linux and macOS providers collect native route and link/medium facts off the
render/event loop and feed fixture-driven parsers. Windows either supplies the
same facts through a supported provider or returns an explicit unsupported/
unknown observation. Interface-name prefixes may be a documented last-resort
hint only; they cannot override route or native medium facts. Collection is
bounded, reuses the metrics sampling cadence, and adds no UI-thread command or
unbounded enumeration.

## Configuration and rendering

The built-in `[stats].net_icon` default becomes automatic. An existing literal
value remains a fixed icon override, preserving user configurations. Automatic
mode selects configurable Wi-Fi, Ethernet, and generic glyphs through the
capability/glyph chokepoint, with explicit ASCII-safe fallbacks. Reload updates
the mode and glyphs without restarting the compositor.

The icon describes selected path semantics; the adjacent rates remain the same
aggregate across non-loopback interfaces. Unknown classification, no traffic,
or a provider failure never hides otherwise valid rate data. Narrow-width
elision follows the existing statusbar policy and never substitutes a false
Wi-Fi/Ethernet glyph.

## Diagnostics

Doctor reports the selected interface and medium, whether the decision came
from default-route or sole-interface evidence, provider support, and the reason
for a generic fallback. It distinguishes quiet connectivity from absent route
facts and does not claim a physical medium from throughput alone.
