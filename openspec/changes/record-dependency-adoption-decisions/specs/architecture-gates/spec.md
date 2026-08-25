# architecture-gates

## ADDED Requirements

### Requirement: Dependency adoption is audited and recorded

The repository SHALL gate its dependency tree with `just deps-audit`
(cargo-deny plus cargo-machete; run by `just ci` and `just lint`), whose
policy lives in `deny.toml`: RustSec vulnerability advisories fail the gate
unless a documented exception carries both a reason and an exit condition;
licenses outside the permissive allowlist fail; wildcard version requirements
and unknown registries or git sources are denied; duplicate major versions
warn, with the known splits named in the `[bans]` comment together with the
intent to ratchet `multiple-versions` to `deny` once they are consolidated.
cargo-machete SHALL fail the gate on a direct dependency no crate uses.

Every direct workspace dependency MUST carry a rationale comment at its
manifest declaration when its presence, pin, or feature selection is not
self-evident — the why, plus any load-bearing constraint (a version pinned to
avoid a duplicate data set, a TLS stack chosen to keep a single rustls, a
feature trimmed to protect the idle loop). A crate evaluated and rejected for
adoption SHOULD have its verdict recorded in an openspec change or manifest
comment so the question is not silently re-asked.

#### Scenario: A disallowed license fails the gate

- **WHEN** a direct dependency whose license is outside deny.toml's allowlist
  is added to a manifest
- **THEN** `just deps-audit` fails the licenses check until the dependency is
  removed or an explicit license exception is added

#### Scenario: An unused direct dependency fails the gate

- **WHEN** a direct dependency remains declared in a crate manifest after the
  last use of it is deleted
- **THEN** `just deps-audit` fails via cargo-machete until the declaration is
  removed

#### Scenario: A new advisory forces an upgrade or a documented exception

- **WHEN** a RustSec vulnerability advisory is published against a crate in
  the lock
- **THEN** `just deps-audit` fails until the dependency is upgraded or an
  ignore entry with a reason and an exit condition is added to deny.toml

#### Scenario: A duplicate major version is surfaced, not silently absorbed

- **WHEN** a manifest change makes the tree resolve two major versions of the
  same crate
- **THEN** cargo-deny reports the split, and the change either aligns the
  direct pin with the in-tree cohort or extends the documented known-splits
  note in deny.toml
