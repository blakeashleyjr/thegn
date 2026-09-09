# Sandbox — optional build cache delta

## ADDED Requirements

### Requirement: Optional compiler caching cannot make a sandboxed build fail

Sandbox composition SHALL resolve one effective compiler-cache policy from an
explicit `[sandbox]` setting: `off` (the default) or fail-soft `auto`.
Inherited dev-shell and `[disk]` cache values MAY provide wrapper/transport
inputs but SHALL NOT enable sandbox cache use. Off SHALL remove inherited
sccache wrapper and endpoint environment and SHALL add no cache-specific grant.
Auto SHALL verify the wrapper and transport through the effective containment;
when that probe is unreachable or unwritable, launch SHALL remove the wrapper,
compile directly, and expose a degradation diagnostic rather than fail solely
because the optional cache is absent.

#### Scenario: The dev shell injects a disabled wrapper

- **WHEN** the inherited environment contains `RUSTC_WRAPPER=sccache` and effective cache policy is off
- **THEN** the sandboxed process receives neither that wrapper nor cache-specific transport grants

#### Scenario: The configured cache is unreachable

- **WHEN** cache policy is auto/on but the contained wrapper cannot reach its transport
- **THEN** the build runs uncached and doctor/status records the fallback reason

#### Scenario: Host reachability does not prove sandbox reachability

- **WHEN** sccache and its endpoint are usable on the host but not through the effective sandbox grants
- **THEN** auto mode falls back to direct rustc and reports the contained failure separately from the successful host check

#### Scenario: The configured cache is reachable

- **WHEN** auto mode's contained probe can execute the wrapper and write through its narrowly granted transport/data path
- **THEN** the sandboxed build uses sccache and a repeated smoke build can observe a cache hit

### Requirement: Doctor explains sandbox build-cache authority

Doctor SHALL report the winning configuration layer, effective mode, wrapper
path, host availability, cache data path, transport endpoint, sandbox grants,
contained reachability/writeability, and whether direct-compiler fallback will
occur without running a full build. An unsupported backend SHALL be reported as
not proven, never as reachable based only on host state.

#### Scenario: Conflicting dev-shell and user settings exist

- **WHEN** inherited wrapper environment conflicts with explicit user config
- **THEN** doctor names both sources and the explicit modeled configuration wins

#### Scenario: The platform cannot run a contained cache probe

- **WHEN** the resolved backend does not support the compiler-cache reachability probe
- **THEN** doctor reports sandbox cache use as not proven and does not convert host availability into a pass
