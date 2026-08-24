## MODIFIED Requirements

### Requirement: Control API drives a running instance

The daemon SHALL expose a control API (HTTP/gRPC plus an SSE/WebSocket event feed) gated by scoped tokens, and `thegn` CLI verbs (open worktree, send-to-terminal, snapshot, drive-browser) MUST drive a running instance through this API, degrading gracefully when no daemon is running; the API transport runs entirely off the render loop and never introduces a polling timeout. The HTTP routes MUST be generated from the `ROUTES` table of the capability catalog so that every route names the capability (and therefore the verb and scope) it serves, and the API MUST include `GET /v1/worktrees`.

#### Scenario: CLI verb reaches the live instance

- **WHEN** the user runs a `thegn` send-to-terminal verb against a running daemon
- **THEN** the input is delivered to the live pane over the control API and reflected in the attached UI

#### Scenario: Scope is enforced

- **WHEN** a client calls a control verb with a token lacking the required scope
- **THEN** the request is rejected without performing the action

#### Scenario: No daemon present

- **WHEN** a `thegn` control verb runs and no daemon is running
- **THEN** the CLI degrades gracefully with a clear message rather than crashing

#### Scenario: Routes are catalog-driven

- **WHEN** the HTTP router is built
- **THEN** every registered route corresponds to exactly one `ROUTES` entry whose capability id exists in the catalog
