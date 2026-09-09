# Design — add-container-management

## Context

What exists (all verified in-tree):

- `thegn_core::sandbox::Backend` — the one profile table (label, binary,
  dialect quirks) the sandbox spec already requires; `backend_prefix()`
  builds per-backend argv prefixes (incl. `sudo -n` for rootful podman).
- `running_containers()` — probes rootless podman, rootful podman, docker;
  parses `ps` JSON/NDJSON (`parse_podman_ps` / `parse_docker_ps`, pure +
  tested); merges `stats --no-stream` via `oci_stats()`; sorts ours-first
  (`CONTAINER_PREFIX`). Runs every 5s on the hydrate refresh ticker
  (background thread, `perf::Subsys::Container` attribution, waker pulse).
- `run_gc(db_worktrees)` — startup orphan sweep over `Backend::all_oci()`
  (the fixed-list leak of rootful podman and apple is already fixed), using
  `identify_orphans` with the agent-container and VPN-sidecar suffix
  reverse-mapping so a live sidecar is never misread as an orphan.
- Host provisioning labels images/volumes `thegn.managed=true` (+
  `thegn.volume.role=…`); `host rm`/`rm-cache` print "prune them there if
  wanted" instead of doing it.
- The Sandbox panel section + container chip show the **active worktree's**
  container (health-bulleted); `model.containers` holds the full list.
- `OciRunner` (thegn-svc) is the local/ssh exec seam every host-side
  container command already rides.

Reference triage: **oxker** = list + logs + stats charts + start/stop/delete
for docker; **lazydocker** = that plus images/volumes/networks, prune
commands, and compose services. The in-lane subset of both is exactly:
per-container stats, logs, lifecycle on _our_ containers, and prune — which
is what this change builds, generalized across thegn's five backends and its
remote hosts, with ownership as a hard boundary neither reference tool has
(they manage everything; thegn deliberately does not).

## Decisions

### D1. Ops on the profile table, not a new seam registry

"Seams, not vendors" is satisfied by extending the existing backend profile
table rather than minting a parallel `ContainerRuntime` provider registry:
the sandbox spec already requires "Backends are described by one profile
table", the doctor probe for sandbox backends already exists, and a second
table would immediately drift from the first (backend added to one, missed
in the other).

Shape: a `ManageOps` capability set per `Backend` (list / stats / df / logs /
control / prune), with pure functions

```
mgmt_list_argv(backend) -> Option<Vec<String>>
mgmt_stats_argv(backend) -> Option<Vec<String>>
mgmt_df_argv(backend) -> Option<Vec<String>>
mgmt_logs_argv(backend, name, tail) -> Option<Vec<String>>
mgmt_control_argv(backend, op, name) -> Option<Vec<String>>   // stop|start|restart|rm
mgmt_prune_argv(backend, kind) -> Option<Vec<String>>         // containers|images|volumes
```

returning `None` where the backend lacks the op (caps ⇔ optional ops). The
apple dialect (`container …`, no `system df`) stays inside its table rows;
docker/podman strings appear nowhere outside these builders. Parsers for the
new outputs (`system df --format json`, logs framing) sit beside
`parse_podman_ps` and are unit-tested against captured fixtures — this is
the 95%-covered pure-logic half; execution is smoke-tested.

`thegn doctor` extends the existing sandbox probe report with the supported
management ops per detected backend.

### D2. Ownership is enforced in the argv builders

The prune/control builders take no free-form filter: `mgmt_prune_argv`
hard-codes `--filter label=thegn.managed=true` (images/volumes) and the
`thegn-` name-prefix family match (containers, applied to the parsed list
before any `rm`, as `identify_orphans` does today — docker/podman name
filters are prefix-capable but the belt-and-braces is filtering the parsed
list we already have). `mgmt_control_argv` refuses (returns `None`) for a
name outside the owned families. A unit test asserts **no** prune argv can
be constructed without its filter and no control argv for a foreign name —
the invariant is structural, not reviewed-for.

Gap to close on the way: locally created sandbox containers are identified
by name prefix but not all carry the `thegn.managed` label; creation adds
the label so containers, images, and volumes converge on one marker (the
name-prefix rule remains for the existing estate). Compose/devcontainer
services created by thegn get the label via the compose override thegn
already writes.

### D3. Placement: a Containers tab in the monitor modal

Considered: a new panel section (rejected — the panel is per-worktree
context; this list is machine-global), a standalone overlay (rejected — the
monitor already owns tabs, history, pause, and the stats drain), the sidebar
(rejected — it's a tree of work, not an ops console). The monitor modal is
the machine-global deep-dive surface; Containers becomes its ninth tab,
hidden like GPU/Power when no engine is detected. The Sandbox panel section
and container chip stay as the per-worktree summary and gain nothing.

Actions on an owned row: `s` stop / `r` restart, `l` logs (tail streamed
into the log viewer path), `Enter` shell-in (opens a pane via the existing
exec path — `docker/podman exec`, compose-aware), `x` remove (confirm;
running containers require a second confirm). All spawn off-loop and report
via the existing status/toast path; failures are surfaced, never swallowed.

### D4. Stats gating (a behaviour change to an unspecced path)

`stats --no-stream` forks a stats sample per engine per tick; on docker it
can take over a second. Today it runs unconditionally every 5s forever.
Following the `ProcSampler` precedent: the ambient tick keeps the cheap `ps`
(list, status, health — what the chip and sidebar need); the stats op runs
only while a surface that displays per-container numbers is visible
(Containers tab, or the Sandbox section's expanded stats), with a minimum
interval, and stops when it closes. The aggregate `df` runs on tab open and
then at a slow cadence (60s) while the tab stays open — it is the most
expensive op (`docker system df` walks layer stores).

### D5. Cleanup verbs

- `thegn sandbox gc` — `run_gc` on demand: sweep every available OCI
  backend, remove containers whose worktree is gone from the DB, print what
  was removed per backend. Exit 0 even when nothing to do; the startup sweep
  is unchanged (and now specced).
- `thegn sandbox prune [--host <name>] [--yes] [--containers|--images|--volumes]`
  — default all three kinds, owned-only. On a TTY: list what would be
  removed (name, kind, size where known) and confirm; `--yes` for scripts;
  `--dry-run` always available. `--host` runs the same label-filtered prune
  argv on the provisioned host via `OciRunner::host_exec` (bounded
  timeouts), closing the "prune them there if wanted" loop; `host rm-cache`
  gains a pointer to it.
- Explicitly not: `system prune`, `--all`, volume pruning without the label,
  or any flag that widens past ownership.

### D6. External doors

Three catalog rows, dispatched like every other verb through
`required_scope`:

| id                   | scope | surfaces | notes                                                       |
| -------------------- | ----- | -------- | ----------------------------------------------------------- |
| `containers.list`    | read  | ALL      | owned + foreign (read-only data), stats included when fresh |
| `containers.control` | write | ALL      | stop/start/restart/logs, owned-only by D2                   |
| `containers.prune`   | admin | ALL      | gc + prune, owned-only by D2; dry-run parameter             |

Admin (not write) for prune: it is destructive and estate-wide, the same
tier as daemon shutdown. The MCP projection lands behind the in-flight MCP
scope-gating work — the rows are declared here; that branch's `--scopes`
gate is what exposes them.

### Alternatives considered

- **bollard (daemon API)** — richer streams (events, live stats) but adds a
  tokio-facing dependency for two of five backends only; apple/smol have no
  daemon socket. The argv op surface keeps every backend on one contract; a
  bollard-backed implementation of the same ops is possible later (AB 349
  stays open, narrowed).
- **A separate `thegn containers` CLI namespace** — rejected: cleanup is
  sandbox-estate maintenance and lands under the existing `sandbox` verbs;
  a top-level namespace would imply the general-purpose manager this change
  declines to be. (`add-cli-namespaces-and-remote-open` owns namespace
  shape; the chosen verbs don't collide with it.)
- **Auto-prune on a timer** — rejected: destructive work on a schedule with
  no one watching is how a warm cache or a paused-but-wanted container dies;
  `[lifecycle]` already owns idle economics with consent-shaped config.
  Cleanup here is explicit and user-invoked only.

## Event-loop / render notes

- All engine subprocesses stay on background threads (ambient tick as
  today; tab-triggered ops on the existing task-spawn path), `Qos::Background`,
  results over channels + waker pulse. Nothing blocks the loop; nothing runs
  before first frame.
- Render damage: monitor tab content is overlay content (`Full` while open,
  as today); the chip/sidebar consume the same ambient list they already do.
- The stats gating (D4) _reduces_ steady-state background work; the perf
  suite's `Subsys::Container` attribution verifies it (idle thegn with the
  monitor closed shows no stats subprocess cost).

## Security

- **Container control is host-level power.** Access to the docker socket (or
  rootful podman via `sudo -n`) is root-equivalent on most hosts. Mitigations:
  (1) ownership enforced structurally in the argv builders (D2) — thegn
  never constructs a command against a foreign container, so a compromised
  or confused caller of these ops cannot reach non-thegn workloads through
  them; (2) external doors are catalog rows with scopes — read for listing,
  write for lifecycle, **admin** for prune — checked by `required_scope`
  like every other verb, never a parallel policy table; (3) rootful podman
  ops run only where the existing probe says `sudo -n podman` is usable —
  no interactive sudo, no privilege prompt from a background thread.
- **Blast radius of prune**: bounded to `thegn.managed`-labelled images/
  volumes and `thegn-`-prefixed container families; dry-run listing before
  a TTY confirm; per-kind flags to narrow further; `--host` prune runs the
  identical filtered argv over the existing bounded-timeout control channel
  and can remove only what provisioning labelled. Worst case is re-paying a
  provision/build, never data loss outside thegn's estate — with one
  exception called out in the confirm text: named volumes seeded by thegn
  can hold user state; volume prune therefore lists volumes by role label
  and skips role labels marked persistent.
- **Sandboxed panes cannot reach these ops**: sandbox profiles do not mount
  the engine socket, and in-TUI actions run in the host process; the only
  programmatic route is the scoped control surface.
- **Logs may contain secrets**: the logs action streams to the local viewer
  only; `containers.control` (write scope) gates it externally precisely
  because logs are read-sensitive even though the op feels read-only.
- No credentials are stored or read by this change; no new config values
  hold secrets.

## Open questions

- Should `containers.list` include foreign containers on external surfaces,
  or owned-only there (foreign visible in the TUI only)? Leaning owned-only
  externally — a remote client has no need to inventory the user's unrelated
  workloads. Decide when the MCP projection lands.
- Aggregate footprint on the masthead (a bytes chip) — deferred until the
  Containers tab shows whether anyone watches the number ambiently.
- Whether `smol` participates in prune (its store model differs); the ops
  table lets it advertise nothing at first — resolve during implementation
  against a live smol host.
