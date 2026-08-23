## 1. Seam foundation (thegn-core, pure)

- [x] 1.1 Add `crates/thegn-core/src/seam.rs`: `BoxFuture`, `ErrorClass`, `SeamError` (with `falls_through` default), `Availability`, `ProbeReport`, `Probe`, `Kind`; register in `lib.rs`
- [x] 1.2 Unit tests for `falls_through` per class, `Availability`/`ProbeReport` serde round-trip (95% gate)

## 2. `config_enum!` reserved marker

- [x] 2.1 Extend the macro in `config.rs` with an optional per-variant `reserved` token; emit `impl seam::Kind`, `"reserved": [...]` in the `x-thegn-enum` extension, and a reserved-specific `Err` from `from_str_validated`
- [x] 2.2 Mark `CiProviderKind::{Drone,Woodpecker,Jenkins,Argo}`, `ForgeKind::{Forgejo,Gitea}`, `MediaBackendKind::Jellyfin`, `SandboxBackend::Wsl` as reserved
- [x] 2.3 Remove `DroneCiConfig`/`WoodpeckerCiConfig`/`JenkinsCiConfig`/`ArgoCiConfig` and their `CiConfig` fields; drop the `[ci.drone|woodpecker|jenkins|argo]` sections from `config/config.toml.example`; `ci.rs` factory arm becomes `k if k.is_reserved() => None`
- [x] 2.4 Tests: macro emits `ALL`/`is_reserved`; strict validate rejects a reserved value with the reserved message; lenient deserialize warns and defaults; `marked_definition_count_is_pinned` still 68; `config_example` tests green

## 3. Seam glue (thegn-svc)

- [x] 3.1 Add `crates/thegn-svc/src/seam/{mod,ladder,router,registry}.rs`: `blocking()`, `Ladder<dyn T>::try_each` / `try_each_sync`, `Router<dyn T>::{fan_out, route}`, `registry::probes(cfg)`
- [x] 3.2 Tests with fake layers/accounts: fall-through on Unsupported/NotInstalled, stop on Auth, fan-out isolates one failing account, id-prefix routing
- [x] 3.3 `impl Probe` for the providers the registry can construct today: `GithubCi`/`GitlabCi` (ci), each `IssueBackend` impl, `CalendarBackend` impls, `GixGit`/`CliGit`, `sandbox::Backend` (over `RuntimeProbe`), media via `thegn_media` resolve; reserved selections report `Unavailable("reserved")`
- [x] 3.4 Generic `seam::test_kind_coverage::<K>(factory)` helper + one instantiation per seam with a `config_enum!` kind (ci, forge, media, sandbox)

## 4. Doctor "Providers" section

- [x] 4.1 `cmd/doctor.rs`: render `registry::probes(cfg)` as a text section and `--json "providers"`; missing-binary entries do not change exit status
- [x] 4.2 `test/smoke.sh`: assert `thegn doctor --json | jq .providers` is a non-empty array with `seam`/`id`/`availability` keys

## 5. Capability catalog (thegn-core)

- [x] 5.1 `control.rs`: add `Verb::ALL`, `Verb::PrStatus` (Read), `Verb::NotifyPush` (Write); extend `required_scope` + its test
- [x] 5.2 Add `crates/thegn-core/src/capability.rs`: `Surface`, `SurfaceSet`, `CapId`, `HostCapability`, `CATALOG`, `SURFACE_GAPS`, `lookup`, `for_surface`, `scope_of`
- [x] 5.3 Tests: every verb has exactly one row; ids unique + snake-dotted; admin caps never list Mcp/Plugin; every gap names a real (cap, surface) pair that the cap lists

## 6. Routes table + `/v1/worktrees` (thegn-svc, thegn-host)

- [x] 6.1 `control/mod.rs`: `WorktreeInfo` wire type; `ControlApi::list_worktrees` with `Unimplemented` default
- [x] 6.2 `daemon/service.rs`: implement `list_worktrees` via `WorkspaceStore::worktrees()` on `spawn_blocking`
- [x] 6.3 `control/routes.rs`: `ROUTES: &[(Method, &str, CapId, Handler)]`; `http.rs::router()` folds it; add `GET /v1/worktrees` handler (`authed(Verb::ListWorktrees)`)
- [x] 6.4 Per-surface coverage tests: `http_routes_cover_catalog` (bidirectional), `grpc_methods_cover_catalog` (cfg `control-grpc`, `GRPC_METHODS` table), `cli_verbs_cover_catalog` (`CLI_VERBS` table in `cmd/session.rs`/`cmd/mod.rs`), `mcp_tools_cover_catalog`, `plugin_host_calls_cover_catalog`; seed `SURFACE_GAPS` with today's gRPC/MCP/plugin/CLI debt
- [x] 6.5 `control/tests.rs`: `list_worktrees` scope test (read ok, under-scoped rejected before DB); `client.rs` gains `list_worktrees`

## 7. App registry (thegn-host)

- [x] 7.1 `apps/registry.rs`: `AppBuilder {id, label, enabled, build}` + `APP_BUILDERS` with the observe entry; `from_config` and `start_slot_tile` iterate it
- [x] 7.2 Tests: ids unique; enabled builders appear in `[apps]` effective order

## 8. Plugin wire v0.2 (thegn-core)

- [x] 8.1 `plugin_api.rs`: `RpcResponse`, `RpcError`, `RpcErrorCode`, `Frame` (untagged), `HostVerb::HostCall` (+ `method_name`), new `EventKind`s, `Contribution.{caps, chord}` defaulted, `PluginSpec`, `PluginMode`; bump `API_VERSION` to 0.2.0
- [x] 8.2 `Config.plugins: Vec<PluginSpec>`; update the `config_tests.rs` fixture (add `command`)
- [x] 8.3 `crates/thegn-core/tests/plugin_api_wire.rs`: schemars snapshot of the wire types vs `docs/api/plugin-api-0.2.json`; `THEGN_UPDATE_SNAPSHOTS=1` regenerates; commit the snapshot
- [x] 8.4 Tests: `Frame` decodes response/legacy message lines; minimal `PluginSpec` parses with defaults; unknown extension point still negotiates
- [x] 8.5 `justfile`: remove `plugin_api` from `cov_ignore`; add tests as needed to hold the 95% gate

## 9. Docs + roadmap

- [x] 9.1 `docs/help/configuration.md`: reserved-kind sentence; `docs/help/daemon-and-sessions.md`: `/v1/worktrees`; `docs/superpowers/specs/control-api.md`: add the route and a "Surface gaps" note pointing at `SURFACE_GAPS`
- [x] 9.2 `tasks.md`: update A.4, A.6, P.198, P.206 to cite this change
- [x] 9.3 Fix `openspec/config.yaml` "no IPC" sentence (it contradicts the control plane)

## 10. Gate

- [ ] 10.1 Run `just ci` (includes openspec-validate) once at the end — _deferred: `just ci` includes the known-broken e2e suite; ran instead: `just quick` on core/svc/host, full `thegn-core` nextest (2431 passed), svc `seam::`/`control::`/`ci::` (+ `--features control-grpc`), host `apps::`/`cmd::session::`/`cmd::doctor::`/`daemon::`/`help::`, `openspec validate --all --strict`, `treefmt --fail-on-change`_
