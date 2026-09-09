# capability-catalog Specification

## Purpose

thegn is driven from outside through several doors — the control HTTP/WS API, gRPC, `thegn` CLI control verbs, the MCP server and plugin host calls. The capability catalog is the one list those doors project: each host capability has a stable id, the control `Verb` whose scope policy governs it, and the surfaces it is exposed on, with per-surface coverage tests and a shrink-only list of documented gaps so the doors cannot drift apart.

## Requirements

### Requirement: One host capability catalog

`thegn-core` SHALL define a single `CATALOG` of host capabilities, each with a stable dotted id (`<domain>.<action>`), the control `Verb` whose scope policy governs it, a one-line summary, the set of surfaces it is exposed on (`Http`, `Grpc`, `Cli`, `Mcp`, `Plugin`), the version it appeared in, and an optional deprecation note. The scope required by a capability MUST be `required_scope(verb)` — the catalog never restates policy.

#### Scenario: Every verb has exactly one catalog row

- **WHEN** the catalog tests iterate `Verb::ALL`
- **THEN** each verb maps to exactly one catalog entry and every entry's id is unique and snake-dotted

#### Scenario: Admin capabilities never reach MCP or plugins

- **WHEN** the catalog tests inspect entries whose required scope is `Admin`
- **THEN** none of them list `Mcp` or `Plugin` among their surfaces

### Requirement: Each surface covers the catalog or documents the gap

Each external surface SHALL be a projection of the catalog: the control HTTP
router MUST be built from a `ROUTES` table keyed by capability id; gRPC
methods, CLI verbs, MCP tools and plugin host-calls MUST each carry a table
mapping to capability ids. A `SURFACE_GAPS` list SHALL record every
(capability, surface) pair a surface does not implement, with a reason, and it
MUST only shrink — enforced by a committed shrink-only allowlist
(`test/surface-gaps-ratchet.txt`, one line per excused pair) pinned against
`SURFACE_GAPS` by a unit test, so adding an excuse fails the build until the
file grows a line with a written reason. `SURFACE_GAPS` SHALL hold only
temporary debt: a surface a capability is deliberately never exposed on MUST
be expressed by narrowing the row's declared surface set, not by a permanent
excuse. When the gap table reaches empty, the pinning test SHALL assert it
stays empty.

#### Scenario: An unrouted capability fails the build

- **WHEN** a capability lists `Http` among its surfaces but no `ROUTES` entry
  carries its id and no `SURFACE_GAPS` entry excuses it
- **THEN** the HTTP coverage test fails naming the capability

#### Scenario: A stale gap fails the build

- **WHEN** a `SURFACE_GAPS` entry names a (capability, surface) pair that the
  surface now implements
- **THEN** the coverage test fails asking for the entry to be removed

#### Scenario: A route for an unknown capability fails the build

- **WHEN** a `ROUTES` entry names an id not present in the catalog
- **THEN** the coverage test fails

#### Scenario: A new excuse fails until the ratchet file grows

- **WHEN** a `SURFACE_GAPS` entry is added without a matching line in
  `test/surface-gaps-ratchet.txt`
- **THEN** the ratchet test fails naming the unratcheted excuse

#### Scenario: A burned excuse must leave the ratchet file

- **WHEN** a `SURFACE_GAPS` entry is removed but its ratchet line remains
- **THEN** the ratchet test fails asking for the line to be deleted

#### Scenario: An empty gap table is pinned empty

- **WHEN** `SURFACE_GAPS` is empty and a change reintroduces an excuse
- **THEN** the pinning test fails — full coverage is the ratcheted floor, not
  an aspiration

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

### Requirement: Coverage is reportable per surface

`thegn api coverage` SHALL print, per surface, the counts of implemented,
stub, excused and declared capabilities plus the list of excused pairs with
their reasons, computed from the catalog and the surfaces' own implementation
tables by pure `thegn-core` logic; `thegn doctor` SHALL print a one-line
summary (cells implemented / declared, gap count). The command is local
introspection like `thegn api list` and requires no daemon.

#### Scenario: The ledger reflects the tables

- **WHEN** `thegn api coverage` runs
- **THEN** each surface's counts equal what the per-surface coverage tests
  compute, and every excused pair is listed with its recorded reason

### Requirement: Proxy control operations are catalog rows

The catalog SHALL carry `mcp_proxy.status` (read scope) and
`mcp_proxy.reload` (write scope), each mapped to its own control `Verb` and
projected on the `Cli`, `Http`, and `Grpc` surfaces per the catalog's
coverage contract. The tools of aggregated third-party upstreams MUST NOT be
minted as catalog rows — the catalog governs thegn's own capabilities; the
proxy's default-deny filter governs the third-party surface.

#### Scenario: Status reads, reload writes

- **WHEN** a control client with only `read` scope invokes the two proxy
  capabilities
- **THEN** `mcp_proxy.status` succeeds and `mcp_proxy.reload` is refused
  before any config re-read

#### Scenario: Upstream tools never enter the catalog

- **WHEN** the catalog tests run with proxy upstreams configured
- **THEN** no catalog row corresponds to an aggregated upstream tool

### Requirement: Stub capabilities remain truthful

A catalog row whose implementation unconditionally answers `Unimplemented`
SHALL carry a `stub` marker and appear distinctly in introspection while it is a
short-lived compatibility bridge. Removing the last `Unimplemented` answer for
a row MUST remove its marker. A capability with no provider or success path
MUST NOT remain indefinitely as a stub: it SHALL be removed until a complete
implementation exists. Because no browser-automation provider or success path
exists, the `browser.drive` catalog row, verb, and transport projections MUST
be absent. Reintroducing the id requires at least one complete provider-backed
implementation and the usual surface coverage.

#### Scenario: browser.drive is not advertised

- **WHEN** clients enumerate capabilities or generated public schemas
- **THEN** `browser.drive` is absent rather than discoverable as an operation
  that always returns `Unimplemented`

#### Scenario: Working preview remains visible

- **WHEN** clients inspect the preview surface after the removal
- **THEN** implemented preview discovery/open/fetch capabilities retain their
  existing catalog and behavior
