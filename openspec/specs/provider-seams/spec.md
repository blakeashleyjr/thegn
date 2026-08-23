# provider-seams Specification

## Purpose

Every substitutable backend in thegn — forge, CI, issue tracker, calendar, media, git, sandbox, editor, remote provider — is a _provider seam_: an object-safe trait with implementations selected by a config `kind`, a caps struct that declares optional operations, a seam error that classifies for degradation ladders, and a probe that `thegn doctor` prints. This spec is the shape every seam converges on so a new provider is an implementation, never a rewrite, and a kind that has no implementation is visibly `reserved` rather than silently accepted.

## Requirements

### Requirement: Provider seams share one vocabulary

`thegn-core` SHALL provide a pure `seam` module (no tokio/termwiz dependency) defining the vocabulary every provider seam uses: a `BoxFuture` alias, an `ErrorClass` classification (`Unsupported`, `NotInstalled`, `NotConfigured`, `Auth`, `Transient`, `NotFound`, `RateLimited`, `Other`), a `SeamError` trait exposing `class()` and a constructor `unsupported(op)`, an `Availability` state (`Ready`, `Degraded`, `Unavailable`), a `ProbeReport` record, a `Probe` trait, and a `Kind` trait (`ALL`, `as_str`, `is_reserved`). Every seam trait SHALL be object-safe: a seam is **sync** (plain `&self` methods) when every implementation is process-bound or wraps its own async client and its callers run on blocking threads (git, forge, sandbox, editor); it is **async** (`BoxFuture` methods) only when a native async client is the primary path and callers are async (control API, issues, calendar, media, remote providers). No seam trait or impl SHALL carry `#[allow(async_fn_in_trait)]`, and provider dispatch SHALL go through trait objects (`Box<dyn T>` / `&dyn T` accessors), never a hand-written per-method delegation enum.

#### Scenario: Errors classify for ladders

- **WHEN** a seam error is asked whether a degradation ladder should fall through past it
- **THEN** `Unsupported`, `NotInstalled` and `NotConfigured` answer true and every other class answers false

#### Scenario: A blocking seam is sync

- **WHEN** a seam's implementations are all subprocess- or block_on-based
- **THEN** its trait uses plain `&self` methods and `Ladder::try_each_sync`

#### Scenario: An async seam is dyn-dispatched

- **WHEN** a new provider implementation is added to an async seam (issue tracker, calendar, media, remote provider)
- **THEN** it is registered by constructing a trait object — no per-method match arm is edited, and `test/async-trait-ratchet.txt` stays empty

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

Every provider implementation SHALL implement `Probe`, returning a `ProbeReport` with the seam name, provider id, availability, serialized caps and notes. A registry SHALL construct every provider the loaded config selects — covering ci, forge, issues, calendar, git, editor, sandbox and media — and collect their reports, and `thegn doctor` MUST print them as a "Providers" section in both text and `--json` (key `providers`) output.

#### Scenario: Doctor lists a reserved selection as unavailable

- **WHEN** config selects a reserved kind and `thegn doctor --json` runs
- **THEN** the `providers` array contains an entry for that seam whose availability is `Unavailable` with a reason naming the reserved kind

#### Scenario: Doctor lists a missing binary as unavailable

- **WHEN** the resolved provider needs a CLI binary that is not on `PATH`
- **THEN** its probe reports `Unavailable` naming the binary, and doctor's exit status is unaffected by that entry

#### Scenario: Editor probe names the winning layer

- **WHEN** the editor resolves through the environment layer
- **THEN** its report has seam `editor`, an id naming the layer (`template`/`tool`/`visual`/`env`/`vi`), and a note carrying `[editor] open_in` and line-jump capability

### Requirement: The managed-provider vocabulary is one enum

The managed-sandbox provider kinds (`[env.<name>.provider] provider`) SHALL be declared once as `config_enum! EnvProviderKind`, and every "is this kind a VPS / native-exec / ssh-reached / scale-to-zero / self-suspending" question SHALL be a method on it; the host provider factory MUST match the enum exhaustively so a new kind without a factory arm fails to compile.

#### Scenario: New provider kind

- **WHEN** a variant is added to `EnvProviderKind`
- **THEN** `provider_for_named` fails to compile until it has an arm

### Requirement: Probe reports conform to one shape

The probe registry's output SHALL satisfy machine-checked shape invariants (`thegn_svc::conformance`): every report names a seam from the known set and a non-empty provider id; every `Unavailable` availability carries a non-empty reason; reserved selections report a reason containing "reserved"; per-account factories (issues, calendar) return a backend exactly for implemented, non-`none` kinds; and two registry runs over the same config agree (probes are cheap, deterministic snapshots — never a network round-trip).

#### Scenario: A malformed probe fails conformance

- **WHEN** a seam's probe reports an unknown seam name, an empty id, or an `Unavailable` with no reason
- **THEN** `conformance::assert_report_invariants` panics naming the offending report

#### Scenario: A missing binary is named

- **WHEN** a CLI-backed provider's binary is absent from `PATH`
- **THEN** its availability is `Unavailable` with a reason containing the binary name
