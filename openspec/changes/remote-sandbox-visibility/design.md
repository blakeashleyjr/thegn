# Design: remote sandbox visibility

## Model

`RemoteHealth` (host, per sandbox key = env + provider id) holds:

- `provider`: state (running / warm / cold / stopped) from the provider API,
  `created_at`, the configured `max_lifetime_secs` (→ time left), keep-awake /
  hold status, and the fleet meter's $/h, accrued and projected cost
  (`fleet_read`, THE-2707).
- `sample`: the latest `remote_health::HealthSample` and the time it was taken.
- `prev_sample`: the sample before it (restart detection needs two).
- `events`: the newest journal rows (below).
- `level`: `ok` / `warn` / `unknown` from `remote_health::level`, raised to
  `warn` by an unacknowledged restart or OOM event or lifetime < 1 h.

The pure parts live in `thegn-core::remote_health` (no I/O): the probe script,
`parse_probe`, `classify`, `warning`, `level`. The host owns sampling,
storage and surfaces.

## The probe

One `/bin/sh -c` exec of `PROBE_SCRIPT` through the provider's exec API
(`Provider::run_exec_within`, 20 s deadline). It is POSIX sh, read-only, has no
package dependency, and every read tolerates a missing file, so a sandbox
without cgroup v2, PSI or a readable kernel log still yields a partial sample.
Output is one `key value` line per fact under a `thegn-health v1` magic line;
the parser refuses output without the magic, caps input at 16 KiB and keeps at
most 5 OOM lines.

Memory used / limit prefers cgroup v2 `memory.current` / `memory.max` (what
the OOM killer enforces) and falls back to `MemTotal - MemAvailable`.

A resident bridge (thegn-musl) can later report the same sample over its
existing channel without a fresh exec; the wire format stays the probe's text
so both paths share the parser.

## Restart and OOM classification

`classify(prev, cur)`:

- Boot id changed → `Restarted`. Cause `LikelyOutOfMemory` when `prev` was at
  ≥ 90% of its memory limit or had OOM kills, else `Unknown`. The kernel log
  of the new boot cannot explain the previous boot, so the previous sample is
  the evidence. Idle sleep, provider recycle and lifetime expiry are told
  apart by provider facts in a later slice: a sprite whose API status went
  cold between samples slept; age ≥ `max_lifetime_secs` is expiry.
- Same boot, OOM-kill lines not in `prev` → `OomKilled` with process names and
  RSS. The kernel log is append-only, so `prev`'s tail aligning with `cur`'s
  head marks the already-seen lines.
- Without a previous sample, OOM kills in the current boot are reported as-is.

`warning(noun, event)` renders the specific one-liner
(`sprite restarted: likely out of memory (3.7 GB of 4.0 GB used before the
restart)`).

## Event journal

A `remote_events` table in the state DB: `(sandbox_key, at_ms, kind, detail
JSON)`, kinds `provision_step` (name, duration, exit), `exec_open`,
`exec_close`, `exec_drop`, `restart`, `oom_kill`, `checkpoint`, `env_up`,
`env_down`, `pull`. Bounded to 500 rows per sandbox and 30 days; written from
the existing provision step stream, the exec session drivers and the sampler.
Schema bump and migration land with the slice that adds the table.

## Polling cadence and cost budget

- Background sampler: one probe per running remote sandbox every 60 s while a
  thegn client is attached, 300 s otherwise; never probes a sandbox the
  provider reports cold or stopped (a probe would wake and bill it).
- Concurrency: one probe in flight per provider account (shares the provider
  control gate); a probe that times out backs the sandbox off to 5 min.
- Exec stream end: when an exec ends without an exit status, the host runs
  one immediate probe; a changed boot id replaces the bare message with the
  restart warning (THE-2710 consumes this).
- Probe cost: one short exec, < 4 KiB of output; provider API reads reuse the
  fleet listing already fetched for `thegn fleet`.

## Failure modes

- Probe cannot run (exec refused, timed out, sandbox gone): sample stays
  `None`, level `unknown`, the error is shown verbatim (sanitized). Never a
  fabricated sample.
- Probe output unrecognised (no magic): `unknown` with "printed no
  thegn-health header".
- Partial facts: shown as `-`; a missing boot id disables restart detection
  for that pair of samples.
- Kernel log unreadable (non-root, restricted dmesg): OOM detection falls back
  to the cgroup `oom_kill` counter rising between samples.

## Surfaces

| Surface                                                                                | Slice           |
| -------------------------------------------------------------------------------------- | --------------- |
| `thegn fleet inspect <sprite> --env <S> --probe [--json]`                              | 1 (this change) |
| `thegn env status [--watch] [--json]` for the current worktree's sandbox, any provider | 2               |
| Restart / OOM warning + notification, stream-end replacement                           | 3               |
| Sidebar remote-row health dot and warning badge                                        | 4               |
| Observe tab remote section (CPU / memory / disk graphs from the sampler)               | 5               |
| Fleet popup health column, control API / MCP `remote.health`                           | 6               |
| Event journal, provisioning / job logs one key away, top processes                     | 7               |
