---
id: sandbox-resources
title: Sandbox resource limits and caches
parent: sandboxing
order: 3
---

# Sandbox resource limits and caches

Part of [[sandboxing]]. CPU, memory and process limits, and the caches a sandbox can reach.

## Resource limits and caches

```toml
[sandbox.limits]
cpu    = "2"
memory = "4g"
memory_high = "85%"        # per-pane throttle line; size or % of memory
memory_swap = "4g"         # per-pane swap cap; "0" = none, "off" = unbounded
memory_swap_total = "off"  # aggregate swap cap for all panes (opt-in)
heavy_slots = 2   # host-local concurrent gate slots; 0 disables
max_load = 0.0     # absolute 1-minute load-average ceiling; 0 disables

[sandbox.volumes]          # OCI backends only
cargo-registry = "/root/.cargo/registry"
node-modules   = "/root/.npm"
```

A process that exceeds the per-pane `memory` cap is killed and the pane stays
open (the shell survives the out-of-memory kill).

When `memory` is set, each pane also gets a `memory_high` line (default `85%` of
`memory`, or an explicit size such as `"20g"`): past it the kernel throttles the
pane and reclaims its memory before the hard `memory` cap has to kill anything.
A pane also gets a `memory_swap` cap (default `4g`) so one runaway pane cannot
fill the machine's swap; `"0"` forbids swap and `"off"` leaves it unbounded. Set
either to `"off"` to disable it. Both apply to interactive panes only, not to
background or gate jobs, which keep the plain `memory` ceiling. `memory_swap_total`
puts an aggregate swap cap on the shared `thegn.slice`; it is off by default.
Invalid values are ignored (that one limit is not applied).

Named volumes survive container recreation, so a package cache does not
cold-start on every build.

Heavy merge and dispatch gates can share a host-local admission policy. Set
`heavy_slots` to cap concurrent gates across thegn processes, and optionally
set `max_load` to wait while the absolute one-minute host load average is at or
above that value. Both default to zero (off). Locks are released automatically
when a gate exits; after a bounded wait, the gate proceeds with a warning.

`[sandbox] compiler_cache = "off"` is the default and is the only switch that
authorizes an sccache wrapper inside containment. It removes inherited sccache
wrappers and endpoint variables even when `[disk].sccache` or the Nix dev shell
sets them; unrelated custom `RUSTC_WRAPPER` values are preserved. Set it to
`"auto"` to let a local bwrap sandbox probe sccache, mount only its data
directory, and use a sandbox-private Unix socket. A failed probe or later
server/transport I/O error degrades visibly to plain rustc instead of failing
the Cargo build; a real compiler error is preserved and is never retried or
reported as cache degradation. `thegn doctor` shows the host inputs and the last contained
reachability/write probe separately. Backends without a proven transport use
plain rustc.
