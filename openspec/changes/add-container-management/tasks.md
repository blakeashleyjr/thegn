# Tasks — add-container-management

## 1. Ops on the profile table (thegn-core)

- [ ] 1.1 `ManageOps` capability set per `Backend` + pure argv builders
      (`mgmt_list/stats/df/logs/control/prune_argv`) beside `backend_prefix`;
      `None` where unsupported (apple: no `system df`; smol: TBD-empty).
- [ ] 1.2 Ownership enforcement in the builders: hard-coded
      `label=thegn.managed=true` filters on prune; owned-family check on
      control names; unit tests asserting no destructive argv exists without
      its filter and no control argv for a foreign name.
- [ ] 1.3 Parsers for `system df` (docker NDJSON / podman JSON) and any new
      list fields, beside `parse_podman_ps`, against captured fixtures;
      95%-line coverage on the new pure logic.
- [ ] 1.4 Add the `thegn.managed` label at local container/volume creation
      (sandbox create, compose override, agent/sidecar spawn) — name-prefix
      rule retained for the existing estate.

## 2. Catalog + scopes (thegn-core)

- [ ] 2.1 New `Verb`s + `required_scope` mappings (list→Read, control→Write,
      prune→Admin) in `control.rs`; catalog rows `containers.list`,
      `containers.control`, `containers.prune` in `capability.rs`
      (SurfaceSet::ALL), with the pinned-count/coverage tests updated.
- [ ] 2.2 Note the MCP projection rides the in-flight MCP scope-gating work;
      wire only the dispatch this change owns.

## 3. Host wiring (thegn-host / thegn-svc)

- [ ] 3.1 Split the ambient tick: keep `ps` at 5s; move `stats` behind a
      visibility gate (min interval, reset on close) following the
      `ProcSampler` pattern; `df` on tab-open + slow cadence. Verify with the
      perf suite's `Subsys::Container` attribution that a closed monitor
      runs no stats subprocesses.
- [ ] 3.2 Execute control/logs/prune ops off-loop via the existing task-spawn
      path; outcomes through status/toast; failures surfaced.
- [ ] 3.3 Host-side prune: run the filtered prune argv via
      `OciRunner::host_exec` with bounded timeouts; hook a pointer into
      `host rm-cache` output.
- [ ] 3.4 Extend the doctor sandbox probe with per-backend supported
      management ops.

## 4. Containers tab (monitor)

- [ ] 4.1 Ninth `MonitorTab::Containers`, hidden when no engine detected;
      list ours-first with stats columns; aggregate footprint header with
      partial-total marking.
- [ ] 4.2 Row actions on owned rows: stop/restart, logs tail into the viewer
      path, shell-in pane via the backend exec path, remove with
      confirm/double-confirm; action ids in `ACTION_SPECS`.
- [ ] 4.3 Update `docs/help/system-monitor.md` and `docs/help/sandboxing.md`
      (help ratchet claims the new action ids).

## 5. CLI verbs

- [ ] 5.1 `thegn sandbox gc`: on-demand `run_gc` with a per-backend removal
      report (exit 0 when idle).
- [ ] 5.2 `thegn sandbox prune [--host] [--yes] [--dry-run]
[--containers|--images|--volumes]`: dry-run listing, TTY confirm,
      persistent-role volume skip with naming; smoke-test coverage in
      `test/smoke.sh`.
- [ ] 5.3 Config example: document any new `[sandbox]` keys (stats gate
      minimum interval, df cadence) in `config/config.toml.example`.

## 6. Validation

- [ ] 6.1 `just e2e-update` for the new monitor tab frames (review diffs);
      pin volatile stats values in `e2e_freeze`.
- [ ] 6.2 Run `just ci` once at the end (includes openspec validate, lint,
      coverage, smoke).
