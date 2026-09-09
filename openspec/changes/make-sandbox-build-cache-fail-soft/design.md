# Design

## One sandbox authority

Add one modeled `[sandbox]` compiler-cache policy with two semantics:
`off` (the default, no contained compiler cache) and `auto` (attempt the
configured sccache and fail soft). `[disk].sccache` and a dev shell may supply
the wrapper, data directory, and transport inputs, but neither can opt a
sandbox into cache use. Normal config precedence and reload resolve the winning
`[sandbox]` value before environment and mount composition.

`off` removes an inherited sccache `RUSTC_WRAPPER` and cache-specific endpoint
variables and adds no cache transport grant. It does not silently rewrite an
unrelated custom compiler wrapper.

## Probe in the effective containment

`auto` resolves the wrapper path, cache directory, UDS/port, and minimum grants,
then runs a bounded readiness/writeability probe through the same effective
sandbox backend and environment as the build. A host-side `sccache` or socket
check is recorded separately and is never accepted as contained proof. The
probe must not expose an arbitrary host socket: only the resolved cache endpoint
and data path may be granted.

When the contained probe succeeds, the launch retains the wrapper. When it is
unavailable, stale, unwritable, or unreachable, launch removes the wrapper and
cache endpoint variables before executing Cargo, records a durable diagnostic,
and runs plain `rustc`. The build result reflects compilation, not optional
cache availability.

## Doctor and smoke evidence

Doctor reports the winning config layer, effective mode, wrapper path, host
availability, cache data path, endpoint, exact grants, contained
reachability/writeability, and whether direct-rustc fallback will occur. It does
not run a full build.

A Linux/bwrap smoke fixture owns its temporary worktree, cache directory, and
endpoint. The reachable case proves a contained compile can contact sccache and
records a cache hit on a repeat build. The unreachable case supplies a broken
endpoint and proves the same compile succeeds with the wrapper removed and a
degradation diagnostic. Unsupported backends report that the contained result
is not proven rather than borrowing the host result.
