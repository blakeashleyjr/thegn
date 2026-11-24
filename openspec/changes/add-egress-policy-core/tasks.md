# Tasks — egress policy core

## 1. Host pattern grammar

- [x] 1.1 Add `HostPattern`, `PatternKind`, `PortSpec` and `PatternError` with
      parsing, normalisation, `matches`, `covers` and `parse_exact_default`.
- [x] 1.2 Validate the canonical (decoded, mapped) name and refuse characters
      outside letters, digits, hyphen, dot and the explicit markers.
- [x] 1.3 Make a portless block entry cover every port.
