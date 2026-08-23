## ADDED Requirements

### Requirement: One forge trait

Every forge operation (pull-request status, list, search, create, merge, draft/auto-merge toggles, reviews, comments, review threads, conversation, diff, checks re-run, issue list/get/create/comment, mention notifications, browser open, caller identity) SHALL go through the object-safe `thegn_core::forge::Forge` trait. Optional operations MUST be declared by a `ForgeCaps` bit and MUST default to `Err(ForgeError::Unsupported)`. Host code MUST obtain a forge from the `ForgeSet` and MUST NOT call the GitHub CLI layer directly; `just lint`'s `forge-leak` ratchet enforces this with an allowlist containing only the forge implementation files.

#### Scenario: Raw gh outside the implementation is rejected

- **WHEN** a host file calls `thegn_core::github::…` or `Command::new("gh")`
- **THEN** `just lint` fails naming the file

#### Scenario: Unsupported op is a typed error

- **WHEN** a forge without the `line_comments` cap is asked to add a line comment
- **THEN** it returns `ForgeError::Unsupported` and never panics

### Requirement: GitHub degrades native to CLI

The GitHub forge SHALL be a ladder of a native (octocrab) layer over the `gh` CLI layer. The native layer MUST answer only the operations it implements and MUST fall through (`NotConfigured`, `Unsupported`) when it has no token, the location is remote, or its circuit breaker is open; `Auth`, `NotFound`, `RateLimited` and `Transient` answers are final and MUST NOT be retried on the CLI layer.

#### Scenario: No token falls through

- **WHEN** `GH_TOKEN`/`GITHUB_TOKEN`/`gh auth token` are all absent and `pr_list` is called
- **THEN** the CLI layer serves the request

#### Scenario: Auth failure is final

- **WHEN** the native layer reports an authentication error
- **THEN** the ladder returns that error without running `gh`

### Requirement: Forge errors are seam errors

`ForgeError` SHALL implement `SeamError`, classifying `NotInstalled`, `NotAuthenticated` (Auth), `NoPr` (NotFound), `RateLimited`, `Offline` (Transient), `Unsupported` and `Other`, and SHALL render a user-facing message via `describe()`. The panel state cached in `pr_cache` MUST be produced only by `PrPanel::from_result`, so transport errors and panel states never round-trip through each other.

#### Scenario: Offline maps to a transient, non-definitive state

- **WHEN** `pr_status` returns `ForgeError::Offline`
- **THEN** the panel state is `Offline`, `is_transient()` is true, and the cache keeps its previous definitive row

### Requirement: Forges are routed per host and probed

A `ForgeSet` SHALL hold one forge per `[[forges]]` entry keyed by host plus the GitHub default, SHALL select by the worktree's `origin` host, and every forge SHALL implement `Probe` so `thegn doctor` lists it. Reserved kinds (`forgejo`, `gitea`) MUST NOT produce a forge; doctor reports them as reserved.

#### Scenario: Default routing without configuration

- **WHEN** no `[[forges]]` are configured
- **THEN** every worktree resolves to the GitHub ladder without invoking git

#### Scenario: Reserved kind is reported, not built

- **WHEN** `[[forges]] kind = "forgejo"` is configured
- **THEN** `thegn doctor --json` lists it as unavailable/reserved and `ForgeSet` holds no entry for it

### Requirement: One identity probe

Caller identity (`whoami`) SHALL be a forge operation; onboarding's forge probe, `thegn doctor`'s auth check and the PR queue's own-PR detection MUST all use it rather than spawning `gh auth status` / `gh api user` themselves.

#### Scenario: Onboarding reports the login

- **WHEN** onboarding probes the forge and `gh` is authenticated
- **THEN** the status carries the login returned by `whoami`

### Requirement: The queue driver is testable with a fake forge

`drive_queue` SHALL take `&dyn Forge`, and the test suite MUST include a fake forge exercising fetch → classify → merge/auto-merge → rerun paths without a network or `gh`.

#### Scenario: Fake forge drives a merge

- **WHEN** `drive_queue` runs against a fake whose PR is green and approved
- **THEN** the fake records a merge (or auto-merge) call and the outcome is `Merged`
