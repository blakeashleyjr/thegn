# Rendering — network transport delta

## ADDED Requirements

### Requirement: Network chrome does not claim an unobserved transport

In automatic mode, the network status element SHALL select Ethernet or Wi-Fi
only from bounded active-interface/default-route facts. A known physical
default route SHALL win; without route data, a sole active known physical
interface MAY select its medium. VPN/tunnel/other default routes, multiple
remaining candidates, missing permissions, unsupported providers, and unknown
medium SHALL use a generic network glyph rather than guessing. Current traffic
volume SHALL NOT determine the medium. The adjacent RX/TX SHALL remain aggregate
non-loopback throughput and SHALL remain visible when classification is unknown
or traffic is zero.

#### Scenario: Ethernet is the default route while Wi-Fi is also up

- **WHEN** both interfaces are active and the default route uses Ethernet
- **THEN** the icon is Ethernet while RX/TX remains aggregate across non-loopback interfaces

#### Scenario: Wi-Fi is the default route while Ethernet is also up

- **WHEN** both interfaces are active and the default route uses Wi-Fi
- **THEN** the icon is Wi-Fi rather than whichever interface currently has more traffic

#### Scenario: A VPN owns the default route

- **WHEN** the observed default route is a VPN, tunnel, or unknown medium and no authoritative underlying physical path is available
- **THEN** the icon is generic rather than an inferred Ethernet or Wi-Fi icon

#### Scenario: A quiet default route stays authoritative

- **WHEN** a known physical default-route interface is connected but its current byte delta is zero
- **THEN** its Ethernet or Wi-Fi icon remains selected and the zero-valued aggregate rate may still render

#### Scenario: Transport classification is unsupported

- **WHEN** sampling cannot determine the active transport on the current platform
- **THEN** the statusbar uses the generic glyph and continues to show bounded aggregate traffic

#### Scenario: A fixed literal override is configured

- **WHEN** the user configures a literal legacy `net_icon` value instead of automatic mode
- **THEN** the renderer uses that fixed icon regardless of detected medium while preserving the existing rate format
