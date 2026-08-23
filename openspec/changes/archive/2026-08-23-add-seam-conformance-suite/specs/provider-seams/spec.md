## ADDED Requirements

### Requirement: Probe reports conform to one shape

The probe registry's output SHALL satisfy machine-checked shape invariants (`thegn_svc::conformance`): every report names a seam from the known set and a non-empty provider id; every `Unavailable` availability carries a non-empty reason; reserved selections report a reason containing "reserved"; per-account factories (issues, calendar) return a backend exactly for implemented, non-`none` kinds; and two registry runs over the same config agree (probes are cheap, deterministic snapshots — never a network round-trip).

#### Scenario: A malformed probe fails conformance

- **WHEN** a seam's probe reports an unknown seam name, an empty id, or an `Unavailable` with no reason
- **THEN** `conformance::assert_report_invariants` panics naming the offending report

#### Scenario: A missing binary is named

- **WHEN** a CLI-backed provider's binary is absent from `PATH`
- **THEN** its availability is `Unavailable` with a reason containing the binary name
