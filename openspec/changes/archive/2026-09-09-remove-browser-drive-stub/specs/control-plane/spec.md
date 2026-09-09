# Control plane — removed browser-driving promise

## MODIFIED Requirements

### Requirement: Control API drives a running instance

The daemon SHALL expose a control API (HTTP/gRPC plus an SSE/WebSocket event
feed) gated by scoped tokens. Implemented `thegn` CLI control verbs (including
open worktree, send-to-terminal, and snapshot) MUST drive a running instance
through this API and degrade gracefully when no daemon is running. The API
transport runs entirely off the render loop and never introduces a polling
timeout. HTTP routes MUST be generated from the `ROUTES` table of the
capability catalog so every route names the capability, verb, and scope it
serves, and the API MUST include `GET /v1/worktrees`. A removed capability id
MUST receive the transport's stable unknown-operation response and MUST NOT
remain discoverable through the catalog or generated schemas.

#### Scenario: CLI verb reaches the live instance

- **WHEN** the user runs a `thegn` send-to-terminal verb against a running
  daemon
- **THEN** the request is authorized by the shared scope table and reaches the
  corresponding live session without blocking the UI render loop

#### Scenario: Scope is enforced

- **WHEN** a client calls a control verb with a token lacking the required scope
- **THEN** the request is rejected without performing the action

#### Scenario: No daemon present

- **WHEN** a `thegn` control verb runs and no daemon is running
- **THEN** the CLI degrades gracefully with a clear message rather than crashing

#### Scenario: Routes are catalog-driven

- **WHEN** the HTTP router is built
- **THEN** every registered route corresponds to exactly one `ROUTES` entry
  whose capability id exists in the catalog

#### Scenario: Removed browser-driving clients fail as unknown

- **WHEN** an older client requests the removed `browser.drive` capability or
  `POST /v1/browser`
- **THEN** it receives the stable unknown-capability or HTTP 404 behavior and
  the operation is absent from gRPC and generated schema discovery
