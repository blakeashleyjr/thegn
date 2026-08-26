# Tasks — add-container-management

## 1. Ops on the profile table (thegn-core)

- [x] 1.1 `ManageOps` capability set per `Backend` + pure argv builders
      (`mgmt_list/stats/df/logs/control/prune_argv`) beside `backend_prefix`;
      `None` where unsupported (apple: no `system df`; smol: TBD-empty).
      _(`sandbox_manage.rs`: `mgmt_list/stats/df/logs/control_argv` +
      `mgmt_image_list/rm`, `mgmt_volume_list/rm`, `mgmt_exec`; `manage_ops`
      derives caps ⇔ builders; apple = list-only, smol = list-only,
      podman/docker = full set. Deviation: no single `mgmt_prune_argv(kind)` —
      prune is list→witness→per-item rm so persistent volumes are skipped and
      images (tag-owned, not labelled) are handled; see the report.)_
- [x] 1.2 Ownership enforcement in the builders: hard-coded
      `label=thegn.managed=true` filters on prune; owned-family check on
      control names; unit tests asserting no destructive argv exists without
      its filter and no control argv for a foreign name.
      _(Structural via `OwnedContainer`/`OwnedImage`/`OwnedVolume` witnesses —
      control/logs/exec/rm take a witness, minted only by ownership-filtered
      claim/parse; volume listing hard-wires `OWNED_LABEL_FILTER`. Tests:
      `owned_container_claim_rejects_foreign_names`,
      `no_control_argv_exists_for_a_foreign_container`,
      `every_owned_volume_listing_carries_the_label_filter`,
      `parse_owned_images_drops_foreign_references`.)_
- [x] 1.3 Parsers for `system df` (docker NDJSON / podman JSON) and any new
      list fields, beside `parse_podman_ps`, against captured fixtures;
      95%-line coverage on the new pure logic.
      _(`parse_system_df`, `parse_owned_images`, `parse_owned_volumes`,
      `parse_size_bytes`/`human_bytes`, `container_running`/`container_health`;
      both engine shapes tested.)_
- [x] 1.4 Add the `thegn.managed` label at local container creation.
      _(`oci_create_opts` labels every OCI container except apple, whose arg
      parser rejects unknown flags; volumes already labelled at provisioning.
      Compose/agent-sidecar labelling deferred — those stay owned by their
      `thegn-` name, which management uses for them.)_

## 2. Catalog + scopes (thegn-core)

- [x] 2.1 New `Verb`s + `required_scope` mappings (list→Read, control→Write,
      prune→Admin); catalog rows `containers.list`, `containers.control`,
      `containers.prune`; pinned-count/coverage tests updated.
      _(Deviation: `containers.prune` is `SurfaceSet::OPERATOR`, not `ALL` —
      it is admin-scoped and the pre-existing `admin_caps_never_reach_mcp_or_plugin`
      invariant forbids admin caps on MCP/plugin, the same shape as
      `daemon.shutdown`. list/control are ALL, gapped on the not-yet-wired
      surfaces.)_
- [x] 2.2 MCP projection rides the in-flight MCP scope-gating work; only the
      dispatch this change owns is wired (CLI `sandbox gc/prune` covers
      `containers.prune` on the Cli surface).

## 3. Host wiring (thegn-host / thegn-svc)

- [x] 3.1 Split the ambient tick: keep `ps` at 5s; move `stats` behind a
      visibility gate (`containers_live`), `df` on a slow sub-cadence while the
      surface is open. `Subsys::Container` attribution retained.
- [x] 3.2 Execute control/logs/prune ops off-loop; outcomes through the monitor
      footer + toast; failures surfaced (`monitor_action`).
- [x] 3.3 Host-side prune via `OciRunner::host_exec` with bounded timeouts;
      pointer hooked into `host rm`/`rm-cache` output.
- [x] 3.4 Extend the doctor sandbox probe with per-backend supported ops.

## 4. Containers tab (monitor)

- [x] 4.1 Ninth `MonitorTab::Containers`, hidden when no engine detected;
      list ours-first with stats columns; aggregate footprint header with
      partial-total marking.
- [x] 4.2 Row actions on owned rows: stop/restart, logs + shell-in as panes,
      remove with confirm/double-confirm.
      _(Deviation: actions live in the self-contained overlay key handler, not
      `ACTION_SPECS` — matching the existing monitor keys; documented in the
      help page prose instead.)_
- [x] 4.3 Update `docs/help/system-monitor.md` and `docs/help/sandboxing.md`.

## 5. CLI verbs

- [x] 5.1 `thegn sandbox gc`: on-demand `run_gc_detailed` with a per-backend
      removal report (exit 0 when idle).
- [x] 5.2 `thegn sandbox prune [--host] [--yes] [--dry-run]
[--containers|--images|--volumes]`: dry-run listing, TTY confirm,
      persistent-role volume skip with naming.
- [~] 5.3 Config example: no new `[sandbox]` keys — the stats gate uses the
  existing 5s cadence and a `CONTAINER_DF_EVERY_TICKS` constant, avoiding
  the `SandboxConfig`/overlay god-file blast radius. (Decision, see report.)

## 6. Validation

- [ ] 6.1 `just e2e-update` for the new monitor tab frames — deferred (e2e is
      known-broken/stale per CLAUDE.md; not run this change).
- [~] 6.2 `just ci` — NOT run (uncommitted for review). Validated with scoped
  `just quick thegn-core`/`thegn-host` + `cargo nextest run -p thegn-core
sandbox_manage`; smoke/coverage/e2e are the reviewer's pre-PR gate.
