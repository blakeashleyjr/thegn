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
heavy_slots = 2   # host-local concurrent gate slots; 0 disables
max_load = 0.0     # absolute 1-minute load-average ceiling; 0 disables

[sandbox.volumes]          # OCI backends only
cargo-registry = "/root/.cargo/registry"
node-modules   = "/root/.npm"
```

A process that exceeds the per-pane `memory` cap is killed and the pane stays
open (the shell survives the out-of-memory kill).

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

## Memory warnings and pane restart

thegn watches each local pane's memory scope about every five seconds. When a
pane reaches 85% of its `memory` cap, or the kernel starts throttling it at the
soft limit, a notification names the worktree and program with the numbers
(`memory 21.3G/24G, swap 3.1G`); it reaches the inbox and desktop like any other
alert. While the pane stays at or above 85% its title carries a
`⚠ mem NN%` badge, cleared once usage falls below 75%. When the kernel kills a
process in the pane's scope, an alert says `OOM-killed a process at 24G`.

If the pane's shell then dies together with an out-of-memory kill, thegn
restarts the pane in the same worktree and layout slot. A pane that was running
Claude or pi gets that agent's own continue command (`claude --continue`,
`pi --continue`, run from the directory the agent was in) typed into the fresh
shell, but only once the pane is in the foreground tab and within two minutes;
for any other program you are left at a plain shell and the status line names
what was running. Nothing reconstructed from a dead process's command line is
ever typed. A pane you closed, or typed into after the kill, or a command that
exited cleanly, is never restarted, and a pane is auto-restarted at most once
every ten minutes. Only local panes with a cgroup v2 scope under `thegn.slice`
are watched.
