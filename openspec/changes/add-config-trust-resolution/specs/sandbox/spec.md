# Sandbox

## ADDED Requirements

### Requirement: A repo `.thegn.*` overlay is a clamped request, not an override

The effective sandbox for a worktree SHALL be resolved by clamping the repo-root
`.thegn.{toml,yaml,yml,json}` `[sandbox]` overlay against the trusted base
(global config plus the active profile overlay). The repo layer, being the
least-trusted authorship layer, MAY only _request within_ the trusted bound: a
constraint field may tighten but never weaken, and a field the repo may not set
is dropped. Every denial MUST be surfaced (a log line, plus a deduped
notification and status on the launch path) and MUST NOT halt the worktree —
resolution continues with the clamped sandbox. The named-env `[env.<name>]
sandbox` overlay is globally defined (trusted) and applies unclamped on top of
the clamped repo base.

#### Scenario: A repo cannot disable the sandbox or choose a backend

- **WHEN** a repo overlay sets `enabled = false` or `backend = "host"`
- **THEN** the request is denied, the effective sandbox keeps the trusted values,
  and a denial is surfaced

#### Scenario: A repo may tighten a constraint

- **WHEN** the trusted network mode is `nat` and a repo overlay sets
  `network = "none"`
- **THEN** the effective network mode becomes `none` (a strict tightening)

#### Scenario: A repo cannot widen egress beyond the trusted ceiling

- **WHEN** the trusted `network_allow` is `["*.github.com"]` and a repo overlay
  requests `["api.github.com", "evil.com"]`
- **THEN** the effective allow-list is `["api.github.com"]` and the uncovered
  entry is denied

#### Scenario: An empty repo allow-list denies all egress

- **WHEN** a repo overlay sets `network_allow = []`
- **THEN** the effective policy denies all egress (a universal DNS block)

### Requirement: Additive repo requests are trust-on-first-use gated

The system SHALL gate additive sandbox requests from a repo overlay (extra
mounts, volumes, `init_script`, `prepare`, `image`, `ports`, `gpu`,
`nix_daemon`): such a request MUST NOT be applied unless a matching approval has
been recorded. An unapproved additive request is surfaced as pending, not
applied, and the worktree still opens. Approval is matched by the request's
canonical form, so a later edit that changes the requested set re-prompts.

#### Scenario: An unapproved mount is not applied

- **WHEN** a repo overlay requests `mounts = ["/etc:/host-etc:ro"]` with no
  recorded approval
- **THEN** the mount is not bound and the request is surfaced as pending

#### Scenario: An approved request applies on the next launch

- **WHEN** the same requested set has been approved
- **THEN** the request is applied at the next worktree launch

### Requirement: A key's resolution is explainable

The system SHALL provide `thegn config explain <key>` reporting the effective
value, the trust layer that set it, and — for `sandbox.*` keys with a repo path
— the clamp trace (which requests were denied or are pending, and why).

#### Scenario: Explain shows why egress was clamped

- **WHEN** `thegn config explain sandbox.network --repo <path>` is run against
  a repo whose overlay requested `network = "host"`
- **THEN** the output shows the effective value, its origin layer, and the denial
  reason for the repo request
