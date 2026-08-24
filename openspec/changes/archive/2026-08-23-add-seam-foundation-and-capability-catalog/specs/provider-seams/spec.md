## ADDED Requirements

### Requirement: Provider seams share one vocabulary

`thegn-core` SHALL provide a pure `seam` module (no tokio/termwiz dependency) defining the vocabulary every provider seam uses: a `BoxFuture` alias, an `ErrorClass` classification (`Unsupported`, `NotInstalled`, `NotConfigured`, `Auth`, `Transient`, `NotFound`, `RateLimited`, `Other`), a `SeamError` trait exposing `class()` and a constructor `unsupported(op)`, an `Availability` state (`Ready`, `Degraded`, `Unavailable`), a `ProbeReport` record, a `Probe` trait, and a `Kind` trait (`ALL`, `as_str`, `is_reserved`).

#### Scenario: Errors classify for ladders

- **WHEN** a seam error is asked whether a degradation ladder should fall through past it
- **THEN** `Unsupported`, `NotInstalled` and `NotConfigured` answer true and every other class answers false

### Requirement: Degradation ladders and multi-account routers are reusable

`thegn-svc` SHALL provide a `Ladder` that runs an operation across ordered layers (native → CLI → unavailable) and returns the first non-fall-through result, and a `Router` that fans an operation out across configured accounts, merging successes and isolating a single account's failure so it never discards the others' results.

#### Scenario: Ladder falls through on an unsupported layer

- **WHEN** the first layer of a ladder returns an error of class `Unsupported` or `NotInstalled` and the second returns `Ok`
- **THEN** the ladder returns the second layer's `Ok`

#### Scenario: Ladder stops on a final error

- **WHEN** the first layer returns an error of class `Auth`
- **THEN** the ladder returns that error without consulting later layers

#### Scenario: One failing account does not poison a fan-out

- **WHEN** a router fans out across three accounts and one returns an error
- **THEN** the merged result contains the two successful accounts' items and the failure is logged

### Requirement: A config kind is implemented or reserved

Every provider `kind` declared with `config_enum!` SHALL mark each value that is accepted but not implemented in this build as `reserved`. The macro MUST emit `Kind::ALL`, `Kind::is_reserved`, and list reserved values in the schema's `x-thegn-enum` extension. `thegn config validate --strict` MUST reject a reserved value with a message that names it as reserved; lenient config load MUST keep today's warn-and-default behaviour. A reserved kind MUST NOT carry a dedicated config sub-table.

#### Scenario: Strict validation rejects a reserved kind

- **WHEN** a config sets `[ci] provider = "drone"` and `thegn config validate --strict` runs
- **THEN** validation fails and the message states that `drone` is reserved (accepted but not implemented)

#### Scenario: Lenient load tolerates a reserved kind

- **WHEN** the same config is loaded by the compositor
- **THEN** load succeeds with a warning and the field takes its default value

#### Scenario: Factory and reserved marker agree

- **WHEN** the kind-coverage test constructs a provider for every value in `Kind::ALL`
- **THEN** the factory returns `Some` exactly for the values that are not reserved

### Requirement: Every configured provider reports a probe

Every provider implementation SHALL implement `Probe`, returning a `ProbeReport` with the seam name, provider id, availability, serialized caps and notes. A registry SHALL construct every provider the loaded config selects and collect their reports, and `thegn doctor` MUST print them as a "Providers" section in both text and `--json` (key `providers`) output.

#### Scenario: Doctor lists a reserved selection as unavailable

- **WHEN** config selects a reserved kind and `thegn doctor --json` runs
- **THEN** the `providers` array contains an entry for that seam whose availability is `Unavailable` with a reason naming the reserved kind

#### Scenario: Doctor lists a missing binary as unavailable

- **WHEN** the resolved provider needs a CLI binary that is not on `PATH`
- **THEN** its probe reports `Unavailable` naming the binary, and doctor's exit status is unaffected by that entry
