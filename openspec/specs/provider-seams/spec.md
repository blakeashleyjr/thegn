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

`thegn-svc` SHALL provide a `Ladder` that runs an operation across ordered layers (native → CLI → unavailable) and returns the first non-fall-through result, and a `Router` that fans an operation out across configured accounts, merging successes and isolating a single account's failure so it never discards the others' results. Account-shaped routers SHALL accept dynamically-provided backends (`push_backend`), so a provider implemented outside the binary — a plugin over the `provider.call` bridge — composes exactly like a configured account.

#### Scenario: Ladder falls through on an unsupported layer

- **WHEN** the first layer of a ladder returns an error of class `Unsupported` or `NotInstalled` and the second returns `Ok`
- **THEN** the ladder returns the second layer's `Ok`

#### Scenario: Ladder stops on a final error

- **WHEN** the first layer returns an error of class `Auth`
- **THEN** the ladder returns that error without consulting later layers

#### Scenario: One failing account does not poison a fan-out

- **WHEN** a router fans out across three accounts and one returns an error
- **THEN** the merged result contains the two successful accounts' items and the failure is logged

#### Scenario: A plugin backend routes like an account

- **WHEN** a plugin issue provider is registered and the router fans out
- **THEN** its results merge under its account label, and its failure is isolated like any account's

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

### Requirement: Authenticated tracker operations share bounded HTTP resources

Linear, Jira, and Kaneo adapters SHALL share one process-wide capacity budget of
eight logical operations. Each logical operation SHALL use one absolute
20-second deadline across capacity admission, request serialization, every
nested request, response streaming, and bounded synchronous decoding. Dropping
the provider future MUST release its capacity and owned response body. A later
request in the same operation MUST NOT restart the deadline or acquire a
second capacity permit.

#### Scenario: a mutation requires several requests

- **WHEN** a provider resolves a status, submits a mutation, and refreshes an issue
- **THEN** all requests use the original operation permit and deadline
- **AND** expiry prevents a subsequent mutation or refresh from being dispatched

#### Scenario: cancellation occurs while waiting or reading

- **WHEN** an operation future is dropped while waiting for capacity or streaming a response
- **THEN** its resources are released so another operation can proceed

### Requirement: Tracker HTTP requests retain their admitted account origin

Authenticated requests SHALL retain the configured scheme, host, effective
port, and base path. Explicit self-hosted HTTP and HTTPS base paths SHALL remain
supported. Redirects, origin/base-path escapes, and response decompression MUST
be disabled. Nonidentity content encodings MUST be refused. Request bodies
SHALL be limited to 512 KiB during serialization and response bodies SHALL be
streamed through a 1 MiB limit before JSON decoding. JSON responses SHALL use a
JSON media type. Diagnostics MUST NOT include credentials, query values, or
arbitrary response bodies.

#### Scenario: a response redirects to another route

- **WHEN** a tracker responds with a redirect
- **THEN** the operation refuses it without sending credentials to the target

#### Scenario: a response streams beyond the body limit

- **WHEN** the response exceeds 1 MiB, including without Content-Length
- **THEN** reading stops with a bounded error and the operation releases its resources

#### Scenario: a configured service uses a base path

- **WHEN** Jira or Kaneo is configured under an HTTP or HTTPS base path
- **THEN** all API requests preserve that admitted base path

### Requirement: Tracker identity syntax is admitted before dispatch

Issue provider adapters SHALL validate configured, caller-supplied, cached, and
provider-returned identities before using them as URL path segments, query
values, CLI options, or plugin control values. Built-in identities SHALL use
bounded provider syntax; plugin native keys SHALL use a separate bounded opaque
UTF-8 budget within a bounded complete control envelope. A malformed scoped identity MUST fail with a parse error and
MUST NOT silently fall back to a bare identifier.

#### Scenario: malformed scoped GitHub identity is refused

- **WHEN** a GitHub id contains an invalid repository or issue number
- **THEN** the adapter returns a parse error before invoking `gh`

#### Scenario: enterprise GitHub authority is preserved

- **WHEN** a provider response URL uses the configured `GH_HOST`
- **THEN** the adapter retains its validated owner/repo scope in the issue id

#### Scenario: plugin key remains opaque

- **WHEN** a plugin returns a bounded Unicode key containing a delimiter
- **THEN** the bridge accepts it in the plugin namespace and applies no
  built-in path-segment grammar to the business key

### Requirement: Control identities use one path encoding

The control client SHALL encode a complete issue identity once as one path
segment, and the control server SHALL decode it once before provider routing.
The implementation MUST NOT split or repeatedly decode the encoded identity.
The server handler SHALL validate the once-decoded value before invoking its
`ControlApi`, and malformed input SHALL produce no provider effect.

#### Scenario: scoped identity remains one control segment

- **WHEN** a caller gets or updates `github:owner/repo#42`
- **THEN** the request path carries one encoded id and the provider receives
  the original identity exactly once
