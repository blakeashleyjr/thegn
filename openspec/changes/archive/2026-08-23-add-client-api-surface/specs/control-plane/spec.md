## ADDED Requirements

### Requirement: PR status and notification push are control verbs

The control API SHALL expose `pr.status` (a read-only projection of the cached per-worktree PR state) and `notify.push` (insert a notification through the same store and routing rules as in-process producers) on HTTP, gRPC and the typed client, with wire types published in the control schema.

#### Scenario: A script pushes a notification

- **WHEN** an authorized client POSTs `/v1/notify` with a title and body
- **THEN** the note lands in the notification inbox subject to the user's routing rules, and the response carries its stored identity

### Requirement: MCP serves scope-gated state tools

`thegn mcp serve` SHALL expose state tools beside the docs tools, gated by `--scopes`: each tool maps to one catalog capability claimed on the MCP surface and is listed/callable only when its `required_scope` is within the requested set; live data comes from the daemon, with a cache fallback where honest, and a clean error naming the daemon when live data is required but unreachable. `MCP_STATE_CAPS` SHALL list exactly the implemented capabilities.

#### Scenario: A scope-excluded tool is refused

- **WHEN** the server runs with `--scopes` that exclude a tool's required scope and a client calls it
- **THEN** the reply is a JSON-RPC error naming the missing scope, and `tools/list` did not advertise it

### Requirement: The catalog is callable generically

`thegn api list` SHALL print the capability catalog (id, surfaces, required scope); `thegn api schema` SHALL emit the control wire schema; `thegn api call <cap>` SHALL resolve the capability's HTTP route from the route table and perform it over the control socket with JSON parameters and output — with no per-verb client code, so a newly routed verb is immediately callable.

#### Scenario: A newly routed verb needs no client change

- **WHEN** a verb gains a route in the `ROUTES` table
- **THEN** `thegn api call <its id>` performs it without any `thegn api` code change

### Requirement: The control wire schema is pinned

The control API's wire types SHALL derive a JSON schema emitted to `docs/api/control-v1.json`, and a snapshot test SHALL fail on any wire change that does not regenerate the file (`THEGN_UPDATE_SNAPSHOTS=1`).

#### Scenario: A silent wire change fails

- **WHEN** a control wire type changes shape without the snapshot being regenerated
- **THEN** the snapshot test fails naming the drift
