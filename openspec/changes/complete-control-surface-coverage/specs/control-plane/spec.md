# control-plane — deltas

## ADDED Requirements

### Requirement: The gRPC mirror covers every capability it declares

Every catalog row that lists `Grpc` among its surfaces SHALL be served by a
gRPC method (proto message + handler adapting `ControlApi`), scope-checked
through `required_scope` before dispatch exactly like HTTP; `GRPC_CAPS` SHALL
list the implemented set and the coverage test pins it. Capabilities
deliberately kept off gRPC (pairing management, daemon shutdown) SHALL be
expressed by their catalog surface declarations, not by excuses.

#### Scenario: A mirrored verb behaves like its HTTP twin

- **WHEN** a client calls the gRPC `MergeAdd` with a token holding `git`
  scope
- **THEN** the worktree's branch is enqueued exactly as via
  `POST /v1/merge/add`, and an under-scoped call is rejected with
  `PermissionDenied` before any action

#### Scenario: gRPC carries no excuse rows

- **WHEN** the gRPC coverage test runs after parity lands
- **THEN** no `SURFACE_GAPS` entry names the `grpc` surface

### Requirement: Daemon shutdown is a routed, scoped verb

The control API SHALL expose `POST /v1/daemon/shutdown` (capability
`daemon.shutdown`, admin scope) performing a graceful daemon shutdown, and
`thegn daemon stop` SHALL drive it over the control socket, degrading with a
clear message when no daemon is running. Local same-uid unix-socket peers
reach it through implicit admin (`[serve] local_admin`); TCP callers MUST
present an admin-scoped token.

#### Scenario: An operator stops the daemon from the CLI

- **WHEN** `thegn daemon stop` runs against a live daemon
- **THEN** the daemon shuts down gracefully and the command reports it

#### Scenario: A non-admin token cannot stop the daemon

- **WHEN** a TCP client whose token lacks `admin` calls
  `POST /v1/daemon/shutdown`
- **THEN** the request is rejected before any shutdown begins

### Requirement: The pairing web-redeem page is served

`GET /pair` SHALL serve a static, fully self-contained HTML page (no external
assets, restrictive CSP) that reads the pairing code from the URL fragment —
so the code never appears in server request logs — redeems it via
`POST /v1/pair`, and shows the minted token exactly once without persisting
it. The page is unauthenticated like `/health`, and the pinned
unauthenticated-route list grows to exactly `/health`, `/pair`, `/v1/pair`.

#### Scenario: The advertised web form works

- **WHEN** a browser opens the `PairingUrl::web_form()` URL
  (`http://host:port/pair#t=tgp1_…`) for a valid unexpired code
- **THEN** the page redeems the code and displays the scoped `tgc1_` token
  once, and the code is absent from the server's request log

#### Scenario: A bad code fails on the page, burning nothing

- **WHEN** the page submits a malformed or already-redeemed code
- **THEN** the redeem is refused with a clear message and no pairing state
  changes

### Requirement: Browser-hosted clients are ordinary paired thin clients

A web client SHALL authenticate exactly like every thin client — redeem a
pairing code, hold a scoped bearer token, answer to `required_scope` — with
no cookie/session or second auth mechanism. Cross-origin access SHALL be off
by default and enabled only by an explicit `[serve] cors_origins` allowlist;
a wildcard origin MUST be rejected at config validation.

#### Scenario: A disallowed origin gets no CORS grant

- **WHEN** a browser script from an origin not in `cors_origins` preflights
  a `/v1` request
- **THEN** the response carries no CORS allowance and the browser blocks the
  call, while non-browser clients are unaffected

#### Scenario: An allowed origin drives the API with its token

- **WHEN** an operator lists a GUI's origin in `cors_origins` and the GUI
  presents a paired token
- **THEN** its `/v1` calls succeed under exactly the token's scopes — no new
  policy surface exists for browsers

### Requirement: Mutating control calls emit audit records

Every control invocation whose required scope is `write`, `git` or `admin`,
and every authentication or scope rejection, SHALL emit one structured audit
record on the tracing target `thegn::control::audit` carrying the caller's
pairing id and label, the capability id, the target resource, and the
outcome. Records MUST never contain a token secret, and emission MUST be free
when no tracing subscriber is installed.

#### Scenario: A commit from a paired phone is attributable

- **WHEN** a paired client with `git` scope performs `git.commit` while a
  tracing subscriber is installed
- **THEN** one audit record is captured naming the pairing id, `git.commit`,
  the worktree, and outcome `ok`

#### Scenario: A refused call is recorded

- **WHEN** a client whose token lacks `write` calls `sessions.input`
- **THEN** the call is rejected and one audit record is captured with outcome
  `no_scope`, containing no secret material
