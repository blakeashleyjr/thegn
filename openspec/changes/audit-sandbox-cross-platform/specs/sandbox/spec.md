# Sandbox

## ADDED Requirements

### Requirement: The enforcement matrix is declared per backend and platform

thegn SHALL maintain one enforcement matrix declaring, for every sandbox
backend on every supported host OS, what is actually enforced: filesystem
isolation, network isolation, resource-ceiling strength (hard / soft / none),
process-tree scoping, and the honest isolation class. Every cell MUST be
derived from the same source-of-truth predicates the resolver uses (the
capabilities derivation, backend suitability, the probed ceiling mechanism) —
never a second, hand-maintained policy table. The matrix MUST be exhaustive by
construction: adding a backend or a host OS without declaring its row fails
the build. `thegn doctor` SHALL render the current host's column, and a cell
MUST NOT claim a mitigation that was not applied on this launch path — an
unverified backend's row carries its verification caveat, and a degraded
ceiling renders as soft, not hard.

#### Scenario: A new backend cannot ship without a matrix row

- **WHEN** a new `Backend` variant is introduced without declaring its
  enforcement row
- **THEN** the test suite fails to compile

#### Scenario: Doctor renders the host column honestly

- **WHEN** `thegn doctor` runs on a Linux host with no cgroup cpu delegation
- **THEN** the matrix column shows the host-toolchain backends' resource
  ceiling as soft (nice), not hard

#### Scenario: An unverified backend's row carries the caveat

- **WHEN** the matrix renders a backend whose runtime verbs have never been
  verified against a real install
- **THEN** its row carries the verification caveat and its isolation class
  under-promises rather than claiming the unverified runtime's theoretical
  class

#### Scenario: No cell claims un-applied confinement

- **WHEN** the `none` backend's row is rendered on a host where thegn applied
  no LSM policy to the pane
- **THEN** the row claims no LSM confinement (no Landlock/Seatbelt mention as
  if in force)

### Requirement: An isolation floor can be demanded and compared honestly

The sandbox SHALL accept `[sandbox] isolation_floor` naming a minimum
isolation class (`shared-kernel`, `userspace-kernel`, or `guest-kernel`;
empty means no floor and preserves today's behavior). When a floor is set, the
resolved launch MUST meet or exceed it, compared over the **honest** isolation
class of what the launch actually enters — after backend-chain selection and
any runtime degrade — so a stronger OCI runtime that degraded to the default
compares as the default's class, and a platform whose containers sit behind a
VM compares as the class the platform honestly provides. A provider placement
is outside the floor's scope: it MUST be reported as `provider-managed` and
MUST NOT be counted as satisfying any floor. A repo `.thegn.*` overlay MAY
only raise the floor or harden the miss policy; a request to lower either
MUST be denied and surfaced through the existing clamp reporting.

#### Scenario: A floor is satisfied by a stronger runtime

- **WHEN** `isolation_floor = "guest-kernel"` and the resolved spec runs an
  OCI backend under an available `krun` runtime
- **THEN** the launch proceeds and its reported class is `guest-kernel`

#### Scenario: A degraded runtime is compared as what it became

- **WHEN** `isolation_floor = "userspace-kernel"` and `oci_runtime = "runsc"`
  degraded to the daemon default because `runsc` is absent
- **THEN** the floor comparison uses `shared-kernel` (the class the launch
  actually has) and the floor is treated as missed

#### Scenario: A repo cannot lower the floor

- **WHEN** the trusted configuration sets `isolation_floor = "guest-kernel"`
  and a repo overlay requests `isolation_floor = "shared-kernel"`
- **THEN** the request is denied, the effective floor stays `guest-kernel`,
  and the denial is surfaced

#### Scenario: A provider placement does not count as a tier

- **WHEN** a floor is set and the worktree resolves to a managed provider
  placement
- **THEN** the launch is reported as `provider-managed` and the floor check is
  bypassed as out of scope rather than counted as satisfied or missed

### Requirement: A floor miss follows the configured policy and fails closed on demand

When the resolved launch cannot meet the configured isolation floor,
`[sandbox] on_floor_miss` SHALL govern the outcome: `degrade` (the default)
launches with the existing degraded flag, a warning naming the floor and the
class actually provided, and a deduped notification; `fail` MUST refuse to
launch the pane — no process spawns on the host — with an actionable error
naming the floor, the best class available on this host, and the remedy.

#### Scenario: Default policy degrades loudly

- **WHEN** `isolation_floor = "guest-kernel"` cannot be met and
  `on_floor_miss = "degrade"`
- **THEN** the pane launches, is flagged as degraded, and the warning names
  the floor and the actual class

#### Scenario: Fail-closed refuses to launch

- **WHEN** `isolation_floor = "guest-kernel"` cannot be met and
  `on_floor_miss = "fail"`
- **THEN** no pane process spawns and the error names the floor, the best
  available class, and how to satisfy the floor

### Requirement: Agent workloads can demand the same floor, and a miss never blames the branch

The shared agent-task engine SHALL support an opt-in to run a queue's agent
task under the resolved sandbox with an `isolation_floor` carrying the same
comparison and miss semantics as interactive launches. The default posture
(host process inside the shared resource slice) is unchanged. When a
fail-closed floor miss or a sandbox setup failure prevents a queue task from
running, the outcome MUST be reported as an infrastructure failure — the queue
entry is held or retried — and MUST NOT be recorded as a failure of the
branch or of the agent's work.

#### Scenario: A sandboxed queue task launches under the floor

- **WHEN** a queue's agent task is configured with the sandbox opt-in and a
  floor the host can satisfy
- **THEN** the task's command runs wrapped by the resolved sandbox's enter
  argv at the demanded class or above

#### Scenario: A floor miss holds the queue entry instead of failing the branch

- **WHEN** a queue agent task's fail-closed floor cannot be met on this host
- **THEN** the task does not run, the queue entry is reported as blocked by an
  infrastructure failure naming the floor, and the branch is not marked failed
