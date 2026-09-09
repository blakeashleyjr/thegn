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

`auto` resolves the wrapper path, cache directory, and minimum grants, then runs
a bounded readiness/writeability probe through the same effective sandbox
backend and environment as the build. The accepted Linux/bwrap transport is a
sandbox-private UDS under its private `/tmp`; the host socket is deliberately
discarded, so the only host grants are the cache data directory and thegn's
narrow compiler-cache state directory. A host-side `sccache` check is recorded
separately and is never accepted as contained proof. Other backends remain not
proven and fall back to plain rustc.

When the contained probe succeeds, the launch replaces the inherited wrapper
with an app-owned fail-soft wrapper and blocks inherited
`SCCACHE_IGNORE_SERVER_IO_ERROR`. The wrapper invokes sccache with its native
ignore-I/O switch disabled. If compilation fails while the same endpoint still
answers a bounded `sccache <compiler> --version` transport probe, the wrapper
preserves that compiler status and does not retry or record a cache degradation.
Only when that transport probe also fails does it record the runtime fallback
and retry the same compiler directly.

When the launch probe is unavailable, stale, unwritable, or unreachable,
composition removes the wrapper and cache endpoint variables, records a durable
decision, and runs plain `rustc`. The build result therefore reflects
compilation, not optional cache availability, without misclassifying real
compiler failures as transport failures.

## Doctor and smoke evidence

Doctor reports the winning config layer, effective mode, wrapper path, host
availability, cache data path, endpoint, exact grants, contained
reachability/writeability, and whether direct-rustc fallback will occur. It does
not run a full build.

An explicitly invoked Linux/bwrap smoke fixture owns temporary cache, output,
state, and endpoint paths while exercising the repository's fully resolved
sandbox. The reachable case proves a contained compile can contact sccache and
records a cache hit on a repeat build. It also proves a genuine compiler error
is preserved without a retry or degradation record. The unreachable case uses
the exact product environment with a broken endpoint and proves the fail-soft
wrapper compiles directly and records a degradation diagnostic. Explicit off
proves a real compile receives no wrapper or cache transport variables.
Unsupported backends report that the contained result is not proven rather than
borrowing the host result.
