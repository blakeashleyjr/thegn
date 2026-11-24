# Sprites selected runner

## ADDED Requirements

### Requirement: A project-bound Sprite executes pinned selected Jobs without a double run

The Sprites selected runner SHALL execute only `Recipe::argv` for a pinned
source on the one Sprite bound to the project, for at most the deadline the
route forwards (clamped to the binding's supported maximum). The runner MUST
reserve the attempt number, binding epoch and spend cap in a durable ledger,
allocated under the ledger lock, before any provider call, and MUST write a
dispatch fence before opening the exec socket. The exec socket MUST be opened
exactly once, because the Sprites exec API starts the command from the upgrade
request; a timeout or server error on that upgrade MUST leave the outcome
unknown, and the runner MUST NOT dispatch the same attempt again. The ledger
SHALL record "never ran" only when the provider definitively rejected the
dispatch (an HTTP 4xx on the upgrade) or the transfer failed before dispatch.
A runner error read back through the selected route is indeterminate regardless
of the ledger's own proof, because the route cannot tell what the runner did.

#### Scenario: Deadline stops the owned command tree

- **WHEN** the forwarded deadline passes before the recipe exits
- **THEN** the runner kills the recipe's process group on the Sprite through a
  second idempotent exec, releases the keep-awake hold, reports `TimedOut`, and
  marks cleanup done only when the kill was confirmed (otherwise pending)

#### Scenario: Exit and signal are reported without invention

- **WHEN** the recipe exits with a code, or with a `128 + N` shell signal code
- **THEN** the runner reports `Exited(code)` or `Signalled(N)` respectively, a
  launcher failure (marked on stderr) reads back as indeterminate, and a ledger
  record without a proven exit is never replayed as exit 1

#### Scenario: Upgrade outcome is unknown

- **WHEN** the exec upgrade times out or fails with a server error
- **THEN** the attempt is indeterminate with cleanup attempted or pending, the
  reserved cap stays committed, outputs are fenced, and no second dispatch
  occurs

#### Scenario: Provider rejects the dispatch

- **WHEN** the provider answers the upgrade with an HTTP 4xx such as failed
  authorization
- **THEN** the ledger records the attempt as rejected with no spend committed

### Requirement: The Sprite checkout is the pinned source and nothing more

The runner MUST materialize the snapshot into a unique per-attempt checkout
under a transfer budget separate from the build deadline, read every file back,
list the checkout, and run only when the REAPI root `Directory` digest of what
the Sprite holds equals the pinned source digest. Snapshot capture MUST follow
the worktree's ignore rules (tracked and non-ignored untracked files), MUST deny
version control, thegn state and credential paths at every depth, MUST preserve
the executable bit, and MUST refuse symlinks rather than drop them. After the
attempt settles with no pending cleanup, the runner MUST delete the checkout,
the per-attempt home and the control file; only the project-scoped build cache
MAY remain between attempts. The cache is runner policy and not a job-spec
`caches` entry.

#### Scenario: Warm edit loop

- **WHEN** a dirty tracked file, a deleted file and an allowed untracked file
  are submitted after an earlier attempt
- **THEN** the new checkout holds exactly that tree, its root digest equals the
  pinned digest, the earlier checkout is already deleted, and the project cache
  is the only shared path

#### Scenario: Stray or secret content

- **WHEN** the checkout holds an entry the snapshot does not, or the worktree
  holds `services/api/.env`, an ignored `target/`, or a symlink
- **THEN** a stray entry refuses the attempt before dispatch, secrets and
  ignored output are never captured, and a symlink fails capture explicitly

### Requirement: One project per Sprite, scrubbed environment

A Sprite MUST serve one project: binding a Sprite already bound to another
project MUST be refused locally and by an owner marker on the Sprite before any
file is written. The recipe MUST run under a scrubbed environment (`env -i`)
with a per-attempt `HOME` and a project-scoped `CARGO_HOME` and
`CARGO_TARGET_DIR`, so agent logins and cloud credentials in the Sprite user's
home are not reachable through the environment. Only proxy and CA variables
(`HTTP(S)_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, `SSL_CERT_DIR`) pass through when
set. A declared output under `target/` is read from the project cache's target
directory, where `CARGO_TARGET_DIR` points. The snapshot never carries `id_*`
SSH private keys (public `.pub` keys are allowed).

#### Scenario: Second project reuses the Sprite

- **WHEN** a second project submits to a Sprite the first project holds
- **THEN** the attempt is refused with no write or exec on the Sprite, so the
  first project's inputs and cache are not reachable
