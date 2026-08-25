# Sandbox

## MODIFIED Requirements

### Requirement: Additive repo requests are trust-on-first-use gated

The system SHALL gate additive sandbox requests from a repo overlay (extra
mounts, volumes, `init_script`, `prepare`, `image`, `ports`, `gpu`,
`nix_daemon`) and repo-authored lifecycle hooks (`[hooks]` entries, gated per
event as `hooks.<event>` categories): such a request MUST NOT be applied
unless a matching approval has been recorded. An unapproved additive request
is surfaced as pending, not applied, and the worktree still opens — an
unapproved hook in particular MUST NOT run and MUST NOT block the lifecycle
operation it is attached to. Approval is matched by the request's canonical
form, so a later edit that changes the requested set re-prompts.

#### Scenario: An unapproved mount is not applied

- **WHEN** a repo overlay requests `mounts = ["/etc:/host-etc:ro"]` with no
  recorded approval
- **THEN** the mount is not bound and the request is surfaced as pending

#### Scenario: An approved request applies on the next launch

- **WHEN** the same requested set has been approved
- **THEN** the request is applied at the next worktree launch

#### Scenario: An unapproved repo hook neither runs nor blocks

- **WHEN** a repo `.thegn.toml` declares `[hooks] pre_destroy` entries with no
  recorded approval and the user deletes the worktree
- **THEN** the hooks do not run, the removal proceeds, and the request is
  surfaced as pending
