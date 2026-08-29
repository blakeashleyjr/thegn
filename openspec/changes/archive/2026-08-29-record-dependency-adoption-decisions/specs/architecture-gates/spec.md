# architecture-gates

## ADDED Requirements

### Requirement: Dependency adoption is audited and recorded

The repository SHALL gate its dependency tree with `just deps-audit`
(`cargo deny check` followed by `cargo machete`, as defined at
`justfile:455-462`; included by `just ci` and the dedicated CI job). Its policy
is `deny.toml`: RustSec advisories fail unless an exception has a documented
reason and exit condition; licenses outside the allowlist fail; wildcard
requirements and unknown registry or git sources are denied; duplicate major
versions warn under the documented known-splits policy. `cargo machete` SHALL
fail the gate on an unused direct dependency.

Every direct workspace dependency MUST carry a nearby rationale comment when
its presence, pin, or feature selection is not self-evident. A candidate
evaluated and rejected or deferred for adoption SHOULD have its verdict
recorded in an ADR or OpenSpec change so the question is not silently
re-asked.

#### Scenario: A disallowed license fails the gate

- **WHEN** a direct dependency whose license is outside `deny.toml`'s
  allowlist is added
- **THEN** `just deps-audit` fails until the dependency is removed or explicitly
  excepted

#### Scenario: An unused direct dependency fails the gate

- **WHEN** a direct dependency remains after its last use is deleted
- **THEN** `just deps-audit` fails via cargo-machete until the declaration is
  removed

#### Scenario: A new advisory forces an upgrade or exception

- **WHEN** a RustSec advisory affects a crate in the lock
- **THEN** `just deps-audit` fails until the dependency is upgraded or a
  documented exception with an exit condition is added

#### Scenario: A duplicate major version is surfaced

- **WHEN** a manifest change creates another major version of a crate
- **THEN** cargo-deny reports the split and the change either aligns the direct
  pin or extends the documented known-splits note
