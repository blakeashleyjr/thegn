# Remote sandbox visibility

## Why

Remote sandboxes fail silently. A sprite that rebooted mid-build (most likely
out of memory) surfaced only as `exec stream ended without an exit status`; a
Vercel sandbox OOM-killed rustc with nothing shown. Provisioning steps,
keep-awake state, lifetime and cost are scattered across separate commands.
The owner asked (THE-2712, 2026-11-25) for "supreme visibility" on remote
sandboxes: live health, resources, events and cost, at a glance and one key
away.

## What Changes

- One `RemoteHealth` model per remote sandbox, fed by the provider API (state,
  lifetime, cost) and a low-overhead in-sandbox probe (one short read-only
  `sh -c` exec: boot id, uptime, load, cores, meminfo, cgroup v2 memory and
  `oom_kill` counter, memory PSI, `df /`, kernel OOM-kill lines).
- Restart and OOM classification from two samples: a changed boot id is a
  restart (likely out of memory when the previous sample was at ≥ 90% of the
  limit or had OOM kills); new kernel OOM-kill lines in the same boot name the
  killed process and its RSS. Warnings are specific
  (`sprite out of memory: kernel killed rustc 3.7 GB`), never "stream ended".
- An event journal per sandbox in the state DB (provision steps with
  durations, exec open/close/drop, restarts, OOM kills, checkpoints, env
  up/down, pulls), bounded and pruned.
- Surfaces: `thegn fleet inspect --probe` and `thegn env status [--watch]`
  (text and JSON), the sidebar remote-row health dot, a remote section in the
  Observe tab, the fleet popup, the control API / MCP, and notifications for
  OOM, restart and expiry-soon.
- First slice delivered with this change: the pure model and classifier in
  `thegn-core::remote_health` and
  `thegn fleet inspect <sprite> --env <S> --probe [--json]`.

## Impact

- Code: `crates/thegn-core/src/remote_health.rs` (new, pure),
  `crates/thegn-host/src/cmd/fleet_inspect.rs`; later the host sampler,
  sidebar row, Observe tab, notifications and a state-DB journal table
  (schema bump in a later slice).
- Docs: `docs/help/environments.md`, `docs/help/cli.md`.
- Related: THE-2710 (sprite restart surfaced as stream end), THE-2711 (OOM),
  THE-2707 (cost meter). This change supplies the classifier those issues'
  surfaces call; it does not change the stream-end path itself.
- Cost: the probe is one exec per sample; it wakes a cold sprite, so the
  background sampler only probes sandboxes that are already running.
