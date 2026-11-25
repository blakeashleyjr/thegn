# Tasks

## 1. Model and one-shot probe

- [x] 1.1 `thegn-core::remote_health`: probe script, `parse_probe`,
      `classify` (restart by boot id, OOM kills by kernel log), `warning`,
      `level` — **unit tests**: full and partial probes, garbage refused,
      cgroup-vs-meminfo memory, OOM line table, classification table.
- [x] 1.2 `thegn fleet inspect <sprite> --env <S> --probe [--json]`: one exec
      probe, live CPU load / memory / disk / uptime / boot id, lifetime left,
      warnings; probe failure reported as `unknown` with the reason —
      **unit tests**: lifetime table, report from a probe and from each
      failure, clap parse.
- [x] 1.3 Help: `environments.md` (Live health), `cli.md`.

## 2. `thegn env status`

- [ ] 2.1 `thegn env status [--watch] [--json]` for the current worktree's
      remote sandbox on any exec-capable provider (Sprites, Vercel, VPS, Fly,
      machine0), via `Provider::run_exec_within`; `--watch` re-probes every
      5 s and prints restart / OOM events as they are classified.
- [ ] 2.2 `fleet inspect --probe` for non-sprites providers (provider-neutral
      id resolution from the fleet listing).

## 3. Restart detection and warnings

- [x] 3.1 Host sampler: per running remote sandbox, cadence and back-off per
      design; keeps `prev_sample` / `sample`; never probes a cold sandbox.
- [x] 3.2 On exec stream end without an exit status, run one probe; a
      changed boot id replaces the message with `warning(...)` (with THE-2710).
- [x] 3.3 Notifications for restart, OOM kill and lifetime < 1 h (once per
      event, through the notifications producer).
- [x] 3.4 Restart cause from provider facts: idle sleep (API status cold
      between samples), lifetime expiry, provider recycle.

## 4. Sidebar

- [x] 4.1 Remote worktree row: health dot (`ok` / `warn` / `unknown`) and a
      warning badge from the sampler's latest level (read from
      `remote_health::global()`; slice 3's sampler feeds it) — **unit tests**:
      dot per level, clipped warning.

## 5. Observe tab

- [x] 5.1 Remote section: CPU load, memory used / limit and pressure, disk,
      from the sampler's ring of samples — **unit tests**: sparklines at five
      widths, empty without a reading, error shown without a sample.

## 6. Fleet popup, API, MCP

- [x] 6.1 Fleet popup health columns (`mem`, `up`, `life`) and warnings block;
      drill-in stays open (needs the journal, 7.x) — **unit tests**: columns
      appear only with a reading.
- [ ] 6.2 Control API / MCP `remote.health` (sample, level, events).

## 7. Journal, logs, processes, cost

- [ ] 7.1 `remote_events` state-DB table (schema bump, v118, landed with 3.1; the
      sampler feeds `restart`, `oom_kill`, `lifetime`) fed by provision
      steps, exec sessions, sampler events, checkpoints, env up/down, pulls.
- [ ] 7.2 Latest provisioning and job logs one key away from the row / popup.
- [ ] 7.3 Top processes by CPU / memory and `thegn job` attempts in the
      probe (bounded `ps` output).
- [ ] 7.4 Cost in the health view from the fleet meter (THE-2707).
