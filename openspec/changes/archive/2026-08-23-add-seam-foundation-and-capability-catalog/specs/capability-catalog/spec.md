## ADDED Requirements

### Requirement: One host capability catalog

`thegn-core` SHALL define a single `CATALOG` of host capabilities, each with a stable dotted id (`<domain>.<action>`), the control `Verb` whose scope policy governs it, a one-line summary, the set of surfaces it is exposed on (`Http`, `Grpc`, `Cli`, `Mcp`, `Plugin`), the version it appeared in, and an optional deprecation note. The scope required by a capability MUST be `required_scope(verb)` — the catalog never restates policy.

#### Scenario: Every verb has exactly one catalog row

- **WHEN** the catalog tests iterate `Verb::ALL`
- **THEN** each verb maps to exactly one catalog entry and every entry's id is unique and snake-dotted

#### Scenario: Admin capabilities never reach MCP or plugins

- **WHEN** the catalog tests inspect entries whose required scope is `Admin`
- **THEN** none of them list `Mcp` or `Plugin` among their surfaces

### Requirement: Each surface covers the catalog or documents the gap

Each external surface SHALL be a projection of the catalog: the control HTTP router MUST be built from a `ROUTES` table keyed by capability id; gRPC methods, CLI verbs, MCP tools and plugin host-calls MUST each carry a table mapping to capability ids. A `SURFACE_GAPS` list SHALL record every (capability, surface) pair a surface does not implement, with a reason, and it MUST only shrink.

#### Scenario: An unrouted capability fails the build

- **WHEN** a capability lists `Http` among its surfaces but no `ROUTES` entry carries its id and no `SURFACE_GAPS` entry excuses it
- **THEN** the HTTP coverage test fails naming the capability

#### Scenario: A stale gap fails the build

- **WHEN** a `SURFACE_GAPS` entry names a (capability, surface) pair that the surface now implements
- **THEN** the coverage test fails asking for the entry to be removed

#### Scenario: A route for an unknown capability fails the build

- **WHEN** a `ROUTES` entry names an id not present in the catalog
- **THEN** the coverage test fails

### Requirement: Worktrees are listable over the control API

The control API SHALL expose `GET /v1/worktrees` (capability `worktrees.list`, read scope) returning the worktrees registered with thegn (path, branch, repo root, remote location descriptor), sourced from the state DB off the render loop.

#### Scenario: List worktrees with a read token

- **WHEN** a client with `read` scope calls `GET /v1/worktrees`
- **THEN** it receives the registered worktrees as JSON

#### Scenario: Under-scoped list is refused

- **WHEN** a client whose token lacks `read` scope calls `GET /v1/worktrees`
- **THEN** the request is rejected before any DB read

### Requirement: Embedded app tiles register through a table

The host SHALL construct embedded app tabs from a static `APP_BUILDERS` registry (id, label, enabled-predicate, builder) rather than hard-coded per-app arms, so adding a tile means adding one registry entry.

#### Scenario: Registry ids are valid tab ids

- **WHEN** the registry test runs
- **THEN** every builder id is unique and every id appears in the effective `[apps]` tab order when its enabled-predicate holds
