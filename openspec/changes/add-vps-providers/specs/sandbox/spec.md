# Sandbox

## ADDED Requirements

### Requirement: A commodity VPS can serve as a managed sandbox provider

superzej SHALL support commodity-VPS vendors (Hetzner Cloud first) as managed
sandbox providers: an `[env.<name>.provider]` with a VPS kind SHALL create the
instance via the vendor's REST API (cloud-init user-data, the managed ssh key,
and superzej identity labels), wait until it is reachable over ssh, and run the
standard provisioning pipeline over an ssh exec/files transport that satisfies
the provider `files` capability. Panes and control-plane git/fs reads MUST
attach through a `szhost vps-ssh <name> --` self-bridge (the default
control/interactive prefix when no `exec_command` is configured) — no vendor
CLI is required. Secrets MUST NOT appear on any command line (host or remote);
provisioning env rides the ssh transport's stdin.

#### Scenario: Provisioning a VPS env yields an interactive pane

- **WHEN** a worktree resolves to a VPS provider env with its API token set and
  is provisioned
- **THEN** an instance is created via the vendor API with the managed key and
  superzej labels, the provisioning pipeline runs over ssh, and the pane
  attaches through the `vps-ssh` self-bridge

#### Scenario: The self-bridge is the default exec prefix

- **WHEN** a VPS provider env has no `exec_command` configured
- **THEN** the placement's control and interactive prefixes are the
  `szhost vps-ssh <resolved-id> --` self-bridge, and chrome git/fs reads and
  the persisted worktree location route through it

#### Scenario: A stale instance IP self-heals

- **WHEN** the attach bridge finds no usable ledger record for an instance
- **THEN** it re-resolves the IP via the vendor API and re-persists the record

### Requirement: VPS instances are leak-safe by construction

superzej SHALL write an intent record to the local instance ledger **before**
the create API call and finalize it (instance id, IP) after; every destroy
SHALL retry transient vendor errors and clear the record. A label-scoped reaper
(instances labeled as superzej-managed **and** created by this host) SHALL
destroy unledgered instances past a grace age, drop ledger records with no live
instance, and enforce the env's `max_lifetime_secs` ceiling. Creates MUST be
refused beyond the env's `max_instances` cap (default 5).

#### Scenario: A crash between create and record cannot leak

- **WHEN** the process dies after the create POST but before the ledger record
  is finalized
- **THEN** a later reaper pass finds the labeled instance without a ledger
  record and destroys it once it exceeds the grace age

#### Scenario: Another host's instances are never reaped

- **WHEN** two superzej hosts share one vendor account
- **THEN** the reaper only considers instances whose host label matches its own

#### Scenario: The instance cap refuses runaway creates

- **WHEN** the ledger already holds `max_instances` managed instances
- **THEN** a further create fails with an actionable error instead of minting
  another billing instance

### Requirement: VPS envs have no checkpoint semantics; speed comes from baked images

Because a stopped VPS still bills, superzej SHALL NOT expose
checkpoint/suspend semantics for VPS providers: the provider's `checkpoints`
capability is absent, the provisioning plan's checkpoint step is skipped, and
warm-pool spares (which record no checkpoint) MUST be destroyed — never
recycled in place — when stale or surplus. `superzej env image-bake` SHALL
build the speed substitute: a throwaway instance runs the repo-independent
provisioning prefix (Nix, direnv; docker via first-boot cloud-init), is
snapshotted and destroyed, and the printed `template = "snapshot:<id>"` makes
later provisions boot from the baked image.

#### Scenario: Stale VPS pool spares destroy instead of recycling

- **WHEN** a VPS warm-pool spare ages past the idle ceiling
- **THEN** the lifecycle destroys it (no restore-in-place path exists)

#### Scenario: image-bake never leaks its throwaway instance

- **WHEN** `superzej env image-bake` finishes — successfully or not
- **THEN** the bake instance is destroyed, and on success the snapshot template
  line is printed for the env config
