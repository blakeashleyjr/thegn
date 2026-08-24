# capability-catalog — deltas

## MODIFIED Requirements

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

## ADDED Requirements

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

### Requirement: Stub capabilities are declared

A catalog row whose implementation unconditionally answers `Unimplemented`
SHALL carry a `stub` marker naming what it waits on; `thegn api list` and the
coverage report MUST present stub rows distinctly so a routed stub never
counts as a working capability. Removing the last `Unimplemented` answer for
a row MUST remove its marker.

#### Scenario: browser.drive reads as a stub

- **WHEN** `thegn api list` runs while `browser.drive` answers
  `Unimplemented` on every surface
- **THEN** the row is presented as a stub, and the coverage report counts it
  under stubs rather than plain implemented
