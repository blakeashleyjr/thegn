# CLI — network classification diagnostics delta

## ADDED Requirements

### Requirement: Doctor explains active-network classification

`thegn doctor` SHALL report provider support, the selected interface/medium,
whether selection came from default-route or sole-interface evidence, and the
reason for a generic fallback. It SHALL distinguish a quiet connected route
from absent or ambiguous platform facts.

#### Scenario: Default-route evidence is available

- **WHEN** doctor observes a supported physical default-route interface
- **THEN** it names the interface, its Ethernet or Wi-Fi medium, and default-route evidence

#### Scenario: Classification is ambiguous

- **WHEN** doctor cannot select a physical medium because the route is VPN/unknown, multiple candidates remain, permissions fail, or the provider is unsupported
- **THEN** it reports the precise fallback reason and the generic outcome rather than claiming Ethernet or Wi-Fi
