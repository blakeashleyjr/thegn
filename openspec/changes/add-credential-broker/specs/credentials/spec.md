# Credentials

## ADDED Requirements

### Requirement: A typed SecretRef is the single secret-reference vocabulary

Every configuration field that names a secret SHALL parse, once at config
load, into a typed `SecretRef` (`keyring:<account>`, `env:VAR`, `file:PATH`,
or a legacy literal), with each field's historic bare-string meaning preserved
explicitly (bare-as-env-name for `api_key_env`-family fields, bare-as-literal
for legacy token fields). The typed form MUST NOT expose a literal's value via
`Display`, `Debug`, or serialization. All ref schemes SHALL be accepted
uniformly on every secret field, including issue-tracker and CI tokens.

#### Scenario: Existing bare env-name config keeps working

- **WHEN** a config has `api_key_env = "FLY_API_TOKEN"`
- **THEN** it parses as an env-var reference and resolves from that variable,
  byte-for-byte as before

#### Scenario: keyring refs work for issue-tracker tokens

- **WHEN** an `[[issues.accounts]]` entry sets `token = "keyring:work-linear"`
- **THEN** the token resolves through the keyring backend at fetch time (today
  this string would be sent to the tracker as the literal token)

#### Scenario: A literal token warns and migrates

- **WHEN** `thegn config validate` runs against a config with a raw token
  pasted in a token field
- **THEN** it warns naming the field and the ref syntax to use, and
  `thegn secret migrate` stores the value (keyring, else a `0600` file),
  rewrites the field to the returned ref via the comment-preserving config
  write path, and the raw value no longer appears in the file

### Requirement: All secret resolution goes through one broker seam

thegn SHALL resolve every secret through a single broker chokepoint backed by
a `SecretStore` provider seam: an object-safe trait with `keyring`, `file`,
and `env` backend kinds implemented and `exec` declared reserved, seam-classed
errors (unavailable / denied / not-found are distinguishable), and a Probe per
backend surfaced in `thegn doctor`. Consumers in substrate-free crates SHALL
receive the resolver by injection. Resolution failures MUST degrade gracefully
(an unavailable keyring falls through to file/env; nothing blocks or wedges a
launch), preserving the existing bounded keyring probe and presence memo.

#### Scenario: One seam serves MCP and the rest of the shell

- **WHEN** both an MCP upstream env ref and a provider token resolve
- **THEN** both go through the same `SecretStore` seam and the same keyring
  service namespace, and no second secret-resolution layer exists

#### Scenario: Doctor shows backends and configured refs

- **WHEN** `thegn doctor` runs
- **THEN** a Secrets section lists one probe row per backend kind (with `exec`
  shown as reserved) and, per configured secret field, its backend and a
  presence-only resolves/missing status — never a value

#### Scenario: Headless host degrades instead of wedging

- **WHEN** a `keyring:` ref is resolved on a host with no usable credential
  store
- **THEN** the broker reports it unavailable within the bounded probe deadline
  and the caller falls back or surfaces an actionable error, without hanging

### Requirement: Secret values never persist in config, argv, logs, or the DB

Secret values MUST NOT appear in configuration plaintext (warned + migratable),
in any process argv (host or remote — subprocesses receive secrets via
environment or stdin only, generalizing the ssh stdin-export discipline), in
logs or tracing output (audit events carry ref names, never values, enforced
by a redaction unit test), or in the SQLite state DB (stored tokens live in
the keyring or `0600` files; existing DB-resident token rows are migrated
out).

#### Scenario: A remote exec carries its secrets off-argv

- **WHEN** a provisioning step sends env values to a managed remote
- **THEN** the values ride the streamed stdin script (or process env), and
  neither local nor remote `ps` output can contain them

#### Scenario: Audit output is provably value-free

- **WHEN** the audit event type is rendered for a fixture containing a known
  sentinel secret
- **THEN** a unit test asserts the sentinel bytes appear in no `Display`,
  `Debug`, or serialized form of the event

### Requirement: Secret access is audited without values

Every broker resolution SHALL emit a structured tracing event (target
`thegn::secret::audit`) carrying the ref name, backend kind, consumer
component tag, and outcome. The instrumentation MUST be free when no
subscriber is installed and MUST NOT add event-loop work or wake sources.
`thegn secret audit` SHALL summarize configured refs with backend and last
probe outcome.

#### Scenario: A resolution is attributable

- **WHEN** the fly provider resolves its API token during provisioning with
  tracing enabled
- **THEN** an audit event records the ref name, the backend that answered,
  `provider:fly` as consumer, and success — and contains no token bytes

### Requirement: Managed SSH keys are scoped and rotatable

thegn-managed SSH keys SHALL support per-provider-account scoping
(`[credentials.ssh] managed_key_scope = "shared" | "per-account"`, default
`shared` for compatibility): with `per-account`, newly provisioned instances
are authorized against a key private to that provider account, while existing
instances keep working against the shared key. `thegn secret ssh rotate` SHALL
generate a replacement key, authorize it on every live instance in scope,
verify connectivity with the new key before de-authorizing the old one, and
leave both keys authorized (reporting so) on partial failure. Instance and
host destroy paths SHALL record which managed key had been authorized there.

#### Scenario: Rotation never bricks a fleet

- **WHEN** `thegn secret ssh rotate` fails to re-authorize one of three live
  instances
- **THEN** the old key remains authorized everywhere, the new key is
  additionally authorized where the step succeeded, and the command reports
  exactly which instances still need the rotation

#### Scenario: Per-account scoping bounds a compromise

- **WHEN** `managed_key_scope = "per-account"` is set and a new DigitalOcean
  instance is provisioned
- **THEN** it authorizes the DigitalOcean account's key, and rotating or
  retiring that key does not affect instances of other provider accounts

### Requirement: SSH host-key verification follows one policy table

Every ssh invocation thegn constructs SHALL name one of four connection
classes — user-declared host, managed fresh instance, loopback over an
authenticated transport, in-sandbox bootstrap — and obtain its host-key
options from a single policy chokepoint: user-declared hosts defer to the
user's own ssh trust configuration; managed fresh instances use accept-new
against a per-instance known_hosts file deleted with the instance; loopback
endpoints tunneled over an already-authenticated transport may disable inner
host-key checking or pin via a host-key alias, with the class recorded;
in-sandbox bootstrap uses accept-new scoped inside the sandbox. No host-key
option literal SHALL appear outside the chokepoint (enforced by a shrink-only
ratchet), and `thegn doctor` SHALL print the class → policy table.

#### Scenario: A new call site cannot invent a fourth policy

- **WHEN** code outside the chokepoint adds a `StrictHostKeyChecking` literal
- **THEN** the host-key ratchet check in `just lint` fails until the site goes
  through the chokepoint with a named class

#### Scenario: Managed-instance pins die with the instance

- **WHEN** a managed VPS is destroyed
- **THEN** its per-instance known_hosts pin is removed and no entry for it was
  ever written to the user's global known_hosts

### Requirement: Agent forwarding and pane secret exposure are explicit policy

SSH agent forwarding SHALL be forced off for managed fresh instances and
loopback-tunneled endpoints, while user-declared hosts keep the existing
configurable default. Sealed and sealed-tunnel sandbox profiles SHALL NOT
expose the SSH agent socket or the user session-bus runtime directory by
default (explicit config may re-add them, and doctor flags it); hardened and
open profiles keep existing defaults. `thegn doctor` SHALL enumerate, per
sandbox tier, exactly which secret-bearing env vars, sockets, and mounts that
tier's effective config would expose to a pane.

#### Scenario: A sealed pane cannot reach the OS keyring or the agent

- **WHEN** a pane runs under the sealed profile with default config
- **THEN** `SSH_AUTH_SOCK` is absent from its environment and the session-bus
  runtime directory is not mounted, so neither the user's ssh-agent nor the
  Secret Service is reachable from inside

#### Scenario: Exposure is visible before it bites

- **WHEN** `thegn doctor` runs with the default hardened config
- **THEN** the exposure listing shows the forge-token env vars, the agent
  socket, and the GPG home mount that a hardened pane would receive

### Requirement: Commit signing binds to the resolved identity with worktree override

A named identity SHALL optionally carry signing configuration (format
`openpgp` or `ssh`, plus a key id or path) that resolves alongside its other
tool bindings into the pane and git-operation environment (`gpg.format`,
`user.signingKey`), following the existing identity scope chain so a
worktree-bound identity overrides workspace and global bindings. The
per-operation signing controls (commit overlay cycle, rewrite signing
override) SHALL continue to layer above the identity default unchanged.

#### Scenario: Two worktrees sign with different keys

- **WHEN** worktree A is bound to identity `release` (ssh-format signing key)
  and worktree B has no binding
- **THEN** commits in A are ssh-signed with `release`'s key while commits in B
  follow the repo/global git config, with no per-commit flags needed

### Requirement: Credential-broker operations are operator-surface catalog rows

Every externally invokable broker operation (set, remove, list, migrate,
audit, ssh-rotate) SHALL be a `thegn_core::capability::CATALOG` row projected
on the operator surfaces only (CLI and control API — not MCP, not plugins),
gated by the single `required_scope(verb)` policy table. Listing operations
return ref names and backends only, and no surface SHALL expose a
secret-value read operation.

#### Scenario: An MCP client cannot enumerate secrets

- **WHEN** the MCP surface projects the capability catalog
- **THEN** no `secret.*` capability appears in its tool list, and the pinned
  surface-set test covers the rows
