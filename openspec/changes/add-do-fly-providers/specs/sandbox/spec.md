# Sandbox

## ADDED Requirements

### Requirement: DigitalOcean is a commodity-VPS provider kind

thegn SHALL support DigitalOcean as a `VpsKind` sharing the entire VPS core
(instance ledger, label/tag-scoped reaper, `thegn vps-ssh` self-bridge, image
bake). An `[env.<name>.provider]` with `provider = "digitalocean"` SHALL create
a droplet via the DigitalOcean REST API tagged as thegn-managed, register
the managed ssh key idempotently, wait until reachable over ssh, and run the
standard provisioning pipeline over the ssh exec/files transport. No vendor CLI
is required.

#### Scenario: A DigitalOcean env provisions over the shared VPS core

- **WHEN** a worktree resolves to a `provider = "digitalocean"` env with its API
  token set and is provisioned
- **THEN** a droplet is created via the DO API with the managed key and
  thegn tags, the pipeline runs over ssh, and the pane attaches through the
  `vps-ssh` self-bridge

#### Scenario: DigitalOcean reuses the VPS leak-safety machinery

- **WHEN** a DigitalOcean droplet is created
- **THEN** an intent record is ledgered before the create call and the
  label/tag-scoped reaper enforces `max_lifetime_secs` and destroys unledgered
  managed droplets — identical to the Hetzner path

### Requirement: Fly.io is a CLI-free managed sandbox provider

thegn SHALL support Fly.io as a first-class `Provider::Fly` (not a
`VpsKind`): an `[env.<name>.provider]` with `provider = "fly"` SHALL create an
app-per-sandbox Machine via the Fly Machines REST API, allocate a dedicated
IPv4 via the Fly GraphQL API, and make the machine reachable over the managed
ssh keypair (public IPv4 + guest sshd) so the provisioning pipeline runs over
the ssh exec/files transport that satisfies the provider `files` capability. No
Fly CLI (`flyctl`) is required; secrets MUST NOT appear on any command line.

#### Scenario: Provisioning a Fly env yields an interactive pane over ssh

- **WHEN** a worktree resolves to a `provider = "fly"` env with its API token
  set and is provisioned
- **THEN** an app + Machine is created via the Machines API with a dedicated
  IPv4, the pipeline runs over ssh to the guest sshd, and the pane attaches
  through the self-bridge — with no `flyctl` invocation

#### Scenario: A baked image boots a machine ready

- **WHEN** a Fly env sets `template = "image:<ref>"` for a published sandbox
  image
- **THEN** the created Machine boots from that image (toolchain + sshd already
  present) instead of building the toolchain on the cold start

### Requirement: Fly envs are scale-to-zero, not destroy-on-idle

thegn SHALL treat Fly as scale-to-zero because a stopped Fly Machine is
near-free (unlike a billing stopped VPS): an idle Fly sandbox is **stopped**
(not destroyed), and start/stop MUST poll the Machine's real state so a
still-transitioning machine is never treated as ready. The env's
`max_lifetime_secs` remains the hard destroy ceiling. A dedicated Fly reaper
(Fly is not a `VpsKind`) SHALL reconcile the `provider = "fly"` ledger records:
destroy a record past `max_lifetime_secs`, and reap a stale-`creating` record
whose create crashed between the intent write and finalize.

#### Scenario: An idle Fly sandbox stops instead of being destroyed

- **WHEN** a Fly sandbox goes idle under its lifetime ceiling
- **THEN** its Machine is stopped (near-free), not destroyed, and a later attach
  starts it again after confirming it reached the running state

#### Scenario: The Fly reaper enforces the spend ceiling

- **WHEN** a `provider = "fly"` ledger record exceeds `max_lifetime_secs`
- **THEN** the Fly reaper destroys the app/Machine (a running machine bills) and
  clears the record

#### Scenario: A crashed Fly create cannot leak

- **WHEN** a Fly `creating` ledger record is older than the stale-creating grace
  age (the create crashed before finalize)
- **THEN** the Fly reaper best-effort destroys the app and drops the record
