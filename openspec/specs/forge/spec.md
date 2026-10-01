# forge Specification

## Purpose

The forge seam: pull requests, reviews, checks, issues and caller identity on a git hosting service, behind one object-safe `Forge` trait. GitHub is served by a native (GraphQL over the shared tracker HTTP client) → `gh` CLI ladder; forges are routed per origin host; host code never calls a vendor CLI directly (enforced by `just lint`).

## Requirements

### Requirement: One forge trait

Every forge operation (pull-request status, list, search, create, merge, draft/auto-merge toggles, reviews, comments, review threads, conversation, diff, checks re-run, issue list/get/create/comment, mention notifications, browser open, caller identity) SHALL go through the object-safe `thegn_core::forge::Forge` trait. Optional operations MUST be declared by a `ForgeCaps` bit and MUST default to `Err(ForgeError::Unsupported)`. Host code MUST obtain a forge from the `ForgeSet` and MUST NOT call the GitHub CLI layer directly; `just lint`'s `forge-leak` ratchet enforces this with an allowlist containing only the forge implementation files.

#### Scenario: Raw gh outside the implementation is rejected

- **WHEN** a host file calls `thegn_core::github::…` or `Command::new("gh")`
- **THEN** `just lint` fails naming the file

#### Scenario: Unsupported op is a typed error

- **WHEN** a forge without the `line_comments` cap is asked to add a line comment
- **THEN** it returns `ForgeError::Unsupported` and never panics

### Requirement: GitHub degrades native to CLI

The GitHub forge SHALL be a ladder of a native GraphQL layer (over the shared tracker HTTP client) above the `gh` CLI layer. The native layer MUST answer only the operations it implements and MUST fall through (`NotConfigured`, `Unsupported`) when it has no token, the location is remote, or its circuit breaker is open; `Auth`, `NotFound`, `RateLimited` and `Transient` answers are final and MUST NOT be retried on the CLI layer.

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

### Requirement: Current-branch PR lookup has explicit checkout scope

Current-branch lookup SHALL distinguish the origin repository, configured PR
base repository, push-head repository and local branch. Native and CLI paths
MUST verify the returned branch and full head repository. The CLI MUST retain
the resolved repository scope when using a discovered PR number. Explicit
number lookups SHALL remain available without a checked-out branch.

#### Scenario: A numeric branch is not a PR number

- **WHEN** the current branch is named `42`
- **THEN** lookup uses it as a head-ref filter, verifies the returned head
  repository, and uses only the resulting verified PR number for detail lookup

#### Scenario: A fork uses separate base and push repositories

- **WHEN** a branch tracks a base repository and explicitly pushes to a fork
- **THEN** current-branch status searches the base for that fork's head while
  repository-wide lists remain scoped to the root checkout's origin

### Requirement: Cached forge rows retain repository provenance

PR panels SHALL carry the checked checkout scope. Publication and subsequent
display MUST reject changed scope or an unproven legacy row. Repository-wide
PR lists SHALL carry their root origin identity, including host and full
namespace. Badge reads MUST reuse the root cache per hydration and MUST NOT
wake remote worktrees merely to display a badge. Scoped My Work feeds MUST
reject an obsolete origin while retaining local tracker rows in freshly
sampled repositories without an origin.

#### Scenario: Origin changes while a provider request is running

- **WHEN** the checkout scope changes during a PR refresh
- **THEN** its result is not published as the new checkout's PR and does not
  emit the old checkout's transition effects

#### Scenario: A partial open-PR list does not authorize cleanup

- **WHEN** a PR is absent from a bounded open list
- **THEN** cleanup still requires a definitive targeted state lookup and
  matching target checkout scope before and after that lookup

### Requirement: Native open-PR pagination is bounded

Native open-PR collection SHALL fetch at most three pages and 300 rows, reject
malformed pages or repeating cursors, and perform no lookup for a zero limit.
A failed page MUST NOT replace a good cache with an apparently empty result.

#### Scenario: A provider repeats its continuation cursor

- **WHEN** consecutive pages repeat a continuation cursor before the requested
  bounded result is complete
- **THEN** collection returns an error instead of continuing indefinitely

### Requirement: Native requests use the matching forge host

The public GitHub native layer SHALL only serve origins on github.com and SHALL
reject unsupported hosts before credential lookup. Enterprise origins SHALL
use a host-aware implementation rather than a same-name public repository.

#### Scenario: Enterprise repository shares a public owner and name

- **WHEN** an enterprise repository has the same owner/name as a public repository
- **THEN** the native public GitHub layer does not query or return that repository

#### Scenario: Foreign URL path resembles a GitHub authority

- **WHEN** a foreign origin contains `@github.com` after its URL authority ends
- **THEN** native admission rejects it before token lookup, using the same strict parse for authority and repository identity
- **AND** supported HTTPS, SSH URL and SCP GitHub origins retain their exact owner and repository identity

### Requirement: Native client error classes determine fallback and connectivity

GraphQL error envelopes, including partial responses with errors, SHALL use the
intended CLI fallback. Server error text SHALL NOT be interpreted as transport
failure based on words in a repository name. Authentication, rate-limit, and
transport errors SHALL preserve their operation classes.

#### Scenario: Repository name contains connect

- **WHEN** GitHub returns a GraphQL error for that repository
- **THEN** the native layer falls through without adding global offline evidence

#### Scenario: Service returns an HTTP error

- **WHEN** GitHub returns an authentication, rate-limit, or server-error response
- **THEN** the operation remains failed and the answer establishes reachability

### Requirement: Credential helpers have bounded output and lifetime

Credential lookup SHALL bound output and time spent awaiting both process exit
and stdout completion. On timeout or output overflow the owned helper group
SHALL be terminated, including descendants retaining stdout. Credential values
and helper stderr SHALL NOT be included in diagnostics.

#### Scenario: Parent exits while a descendant retains stdout

- **WHEN** a helper parent exits and its descendant keeps the output pipe open
- **THEN** lookup times out and the descendant is terminated instead of pinning
  the refresh worker indefinitely

### Requirement: Native client outcomes preserve shared connectivity evidence

The native forge request path SHALL record global reachability for typed
repository, authentication, rate-limit and server answers, while preserving
operation classes and intended fallback. Actual transport errors and request
deadlines SHALL record global failure evidence.

#### Scenario: Partial GraphQL answer follows an offline observation

- **WHEN** automatic connectivity has prior failure evidence and the native GitHub client returns data with GraphQL errors
- **THEN** shared connectivity becomes online and clears its failure count
- **AND** the real fallback ladder invokes its CLI layer exactly once

#### Scenario: Typed final error still proves reachability

- **WHEN** the native GitHub client receives an authentication, rate-limit or server response
- **THEN** shared connectivity records reachability while the typed operation remains failed
- **AND** the fallback layer is not invoked

#### Scenario: Actual transport failure follows successful connectivity

- **WHEN** the native GitHub client transport fails or the request deadline expires
- **THEN** shared connectivity records failure evidence and the operation remains Offline
- **AND** fallback is not attempted as though the native implementation were absent
