# Add container management — first-party stats, lifecycle, and cleanup for the containers thegn creates

Linear: THE-45

## Why

thegn creates containers as a matter of course — per-worktree sandboxes across
five OCI-ish backends (podman rootless/rootful, docker, apple, smol), agent
containers and VPN sidecars with their own name suffixes, compose services via
devcontainer support, and images/volumes on provisioned remote hosts labelled
`thegn.managed=true`. Management of that estate today is real but scattered
and unspecced:

- an ambient container list with stats refreshes every 5s off-thread
  (`sandbox::running_containers()` → Sandbox panel section + container chip),
  running `stats --no-stream` **unconditionally** on every tick even when no
  surface shows the numbers;
- orphan GC (`sandbox::run_gc`) sweeps every OCI backend at startup — but only
  at startup, with no on-demand verb and no report surface;
- teardown is per-worktree (`wt rm`); host-side cleanup is a printed
  suggestion ("images/volumes are labelled thegn.managed — prune them there
  if wanted", `cmd/host.rs`);
- there is no aggregate answer to "how much disk/CPU is thegn's container
  estate costing this machine?" and no lifecycle action (stop/restart/logs/
  shell-in) on a listed container outside the active worktree's chip.

THE-45's reference tools (oxker, lazydocker) show the ceiling. The boundary
this change argues: **first-party management of what thegn creates is clearly
in lane; a docker-desktop replacement is not.** A worktree IDE must be able to
account for, control, and clean up its own containers — across every backend
and host it created them on. Managing arbitrary foreign containers, building
images, browsing registries, or authoring compose stacks is another product;
users who want it run lazydocker/oxker in a pane (`[[tools]]`).

## What Changes

1. **A container-management op surface on the backend profile table** —
   list, stats, disk-usage, logs, stop/start/restart, prune-by-filter as
   capability-flagged optional ops per `Backend`: pure argv builders and
   output parsers in `thegn-core` (the existing `parse_podman_ps` /
   `parse_stats_rows` pattern, unit-tested), subprocess execution in the
   host, vendor dialects confined to the backend table. `thegn doctor`
   reports which management ops each detected backend supports.
2. **Ownership as a hard policy**: control and cleanup apply only to
   thegn-owned resources (the `thegn-` container-name prefix families and
   `thegn.managed` labels). Foreign containers stay visible read-only (ours
   first, as today) and are offered **no** actions. Every destructive argv
   MUST carry the ownership filter by construction.
3. **A Containers tab in the monitor modal**: all containers ours-first with
   per-container CPU/mem/net, status/health and backend; an aggregate
   thegn-footprint header (owned containers/images/volumes — counts and
   bytes, from the engines' disk-usage op); lifecycle actions on owned rows
   (shell-in as a pane, logs tail, stop/restart, remove).
4. **Visibility-gated stats**: the expensive `stats --no-stream` subprocess
   runs only while a per-container-stats surface is visible (and no faster
   than a minimum interval), following the `ProcSampler` precedent; the cheap
   ambient `ps` listing keeps its 5s cadence.
5. **On-demand cleanup verbs**: `thegn sandbox gc` (the startup orphan sweep,
   runnable any time, reporting what it removed) and `thegn sandbox prune`
   (stopped owned containers, `thegn.managed` images/volumes; `--host <name>`
   runs the same label-filtered prune on a provisioned host over its control
   channel; dry-run listing by default on a TTY, `--yes` to execute).
6. **Capability-catalog rows** for the new external doors: `containers.list`
   (read), `containers.control` (write: stop/start/restart/logs), and
   `containers.prune` (admin), projected across CLI/control/gRPC/MCP/plugin
   via `required_scope(verb)` — never a second policy table.

## Non-goals

- Managing, controlling, or pruning containers thegn did not create — under
  any flag. Read-only visibility is the ceiling for foreign containers.
- Image building UI, registry browsing, compose stack authoring, Kubernetes.
- Replacing the CLI substrate with a daemon API client (bollard, AB 349):
  the op surface is deliberately argv-shaped so backends without a daemon API
  (apple, smol) participate; a daemon-API implementation can slot in behind
  the same ops later.
- Container _placement_/runtime tiers — `add-oci-runtime-tiers` owns that.
- Warm/suspend/hibernate lifecycle — `[lifecycle]` owns idle economics; this
  change only adds explicit, user-invoked cleanup.

## Impact

- Roadmap: **AB** 349 (partial: op surface, not bollard), 352
  (spawn/stop/restart — the control half), completes the management story
  around 361/362; **AD** 373 (per-container CPU/MEM), 374 (repo-aggregate
  stats), 375 (per-container stats strip → the Containers tab); the
  container half of **AH 418** (per-container net). AD 376–381 (audit
  logs/timeline) stay open.
- Specs: new `container-management` capability. `sandbox` spec untouched
  (creation/teardown/backends stay where they are; this change consumes the
  same profile table). `capability-catalog`: three new rows (stated in the
  delta; the catalog spec's coverage requirements apply as-is).
- Code (implementation phase): `thegn-core/src/sandbox.rs` +
  `sandbox_backend.rs` (ops + parsers; argv builders with the ownership
  filter), `thegn-core/src/capability.rs` + `control.rs` (verbs/rows),
  `thegn-host/src/hydrate.rs` (stats gating), `monitor.rs` (Containers tab),
  `cmd/` (`sandbox gc`/`prune`), `thegn-svc/src/host` (host-side prune over
  `OciRunner`), `cmd/doctor.rs`.
- Help: `docs/help/system-monitor.md` (Containers tab keys) and
  `docs/help/sandboxing.md` (gc/prune); new action ids claimed (help
  ratchet).
- Related in-flight changes: `add-oci-runtime-tiers` (same backend table —
  additive, no conflict), `add-host-as-resource` / host verbs (`--host`
  prune complements `host rm-cache`), `complete-devcontainer-support`
  (compose-created containers are thegn-owned and must carry the ownership
  marker), `add-sandbox-policy-engine` (policy stays about what runs inside
  sandboxes; this change is about the estate outside them). The in-flight
  MCP write-tools/scope-gating branch is the projection dependency for the
  new catalog rows on the MCP surface.
- No SQLite schema change (owned-container discovery is by prefix/label
  against the engines; the DB worktree registry is already the GC's source).
