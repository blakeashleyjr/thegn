# Sandbox

## ADDED Requirements

### Requirement: Allowlist network mode fails closed

`[sandbox] network` SHALL accept `allowlist` (aliases `filtered` and
`egress-proxy`). While no packet-enforced egress datapath exists, every
resolved launch whose configured network is `allowlist` MUST behave exactly as
`network = "none"`, and every reader of the configured value MUST see it as
`none`; it MUST NOT fall through to `nat` or `host` behavior. The resolved spec
SHALL record that egress filtering was requested, and the policy fingerprint
MUST include that record only when it is set.

#### Scenario: Allowlist launches offline

- **WHEN** a worktree sets `network = "allowlist"`
- **THEN** the resolved sandbox spec has `network = none`, OCI backends receive
  `--network none`, and the spec records that egress was requested

#### Scenario: Existing fingerprints are unchanged

- **WHEN** a spec does not request egress filtering
- **THEN** its policy fingerprint is identical to the fingerprint before this
  change
