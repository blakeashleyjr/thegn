## Why

An architecture audit (2026-08-22) found that thegn's substitutability and extensibility goals are real in intent but uneven in code: ~30 provider seams use three different abstraction idioms, `kind` config values exist with no implementation behind them (Drone/Woodpecker/Jenkins/Argo CI, Forgejo/Gitea forges, Jellyfin media, WSL sandbox), the three external surfaces (control HTTP/gRPC, MCP, plugin `HostVerb`s) each keep their own verb list (the gRPC adapter already lags HTTP, and `Verb::ListWorktrees` has no route at all), and the plugin wire (v0.1) cannot express a request/response. Every later convergence step (forge unification, git backend selection, editor seam, plugin runtime, provider-as-plugin) needs one shared foundation first, so this change lays it — additively, with nothing migrated onto it yet.

## What Changes

- **Provider seam foundation** in `thegn-core` (`seam.rs`, pure): `BoxFuture`, `ErrorClass`, `SeamError`, `Availability`, `ProbeReport`, `Probe`, `Kind`. In `thegn-svc` (`seam/`): `blocking()`, `Ladder<dyn T>` (native → CLI → unavailable), `Router<dyn T>` (multi-account fan-out with per-account failure isolation, generalised from `IssueRouter`), and a `registry::probes(cfg)` that every configured provider reports into.
- **`config_enum!` gains a `reserved` marker**: a kind value may be declared accepted-but-unimplemented. The macro emits `impl Kind` (`ALL`, `as_str`, `is_reserved`), lists reserved values in the `x-thegn-enum` schema extension, and `config validate --strict` rejects reserved values with a message naming them as reserved. Existing phantom kinds are marked: `CiProviderKind::{Drone,Woodpecker,Jenkins,Argo}`, `ForgeKind::{Forgejo,Gitea}`, `MediaBackendKind::Jellyfin`, `SandboxBackend::Wsl`. Their dead config sub-tables (`[ci.drone|woodpecker|jenkins|argo]`) are removed. **BREAKING** (pre-alpha): a config naming a reserved kind now fails strict validation (lenient load still warns and falls back to the default, as today).
- **`thegn doctor` gains a "Providers" section** (text and `--json "providers"`) listing every seam → resolved provider → availability/caps, driven by the registry.
- **Host capability catalog** in `thegn-core` (`capability.rs`): one `CATALOG` of `HostCapability { id, verb, summary, surfaces, since, deprecated }` plus `SURFACE_GAPS` (documented, shrink-only). `Verb::ALL` is added, plus new verbs `PrStatus` (read) and `NotifyPush` (write) so the catalog can name them now.
- **Control routes become a table** (`thegn-svc/src/control/routes.rs` `ROUTES`) that builds the axum router; per-surface coverage tests assert HTTP, gRPC, CLI, MCP and plugin projections cover the catalog or list the gap. `GET /v1/worktrees` is added (`ControlApi::list_worktrees`), closing the existing verb-without-route gap.
- **Embedded app registry** (`thegn-host/src/apps/registry.rs` `APP_BUILDERS`) replaces the hard-coded `observe` arms in `AppHost::from_config` / `start_slot_tile`.
- **Plugin wire v0.2** (additive; `API_VERSION` 0.1.0 → 0.2.0): `RpcResponse`, `RpcError`/`RpcErrorCode`, `Frame` (message | response), `HostVerb::HostCall`, new `EventKind`s, `Contribution.{caps, chord}`, `PluginSpec` (manifest + `command`, `cwd`, `env`, `timeout_secs`, `scopes`, `mode`, `enabled`); `Config.plugins: Vec<PluginSpec>`. A schemars snapshot of the wire types is committed and a test fails when the types change without a version bump. `plugin_api` leaves the coverage-ignore list.

## Capabilities

### New Capabilities

- `provider-seams`: the canonical shape every provider seam converges on — object-safe traits, caps ⇔ optional ops, `Unsupported` convention, ladder/router degradation, `kind ⇔ implemented-or-reserved`, probe reporting via `thegn doctor`.
- `capability-catalog`: the single host-capability list that control API routes, gRPC methods, CLI verbs, MCP tools and plugin host-calls are projections of, with documented per-surface gaps and least-privilege invariants.
- `plugin-api`: the versioned plugin wire contract — request/response framing, a loadable `PluginSpec`, and a committed schema snapshot that ties `API_VERSION` to the wire types.

### Modified Capabilities

- `control-plane`: routes are table-driven from the catalog; `GET /v1/worktrees` is added.

## Impact

- Crates: `thegn-core` (new `seam.rs`, `capability.rs`; `config.rs` macro; `config_ci.rs`, `config_forge.rs`, `config_media.rs`, `config.rs` SandboxBackend; `control.rs`; `plugin_api.rs`; `config_validate.rs`), `thegn-svc` (new `seam/`, `control/routes.rs`; `control/{mod,http,grpc,client}.rs`; `ci.rs`), `thegn-host` (`cmd/doctor.rs`, `daemon/service.rs`, `apps/{mod,registry}.rs`), `justfile` (`cov_ignore`), `config/config.toml.example` (`[ci.*]` removal, `[[plugins]]` note).
- Pinned counts move deliberately: `config_enum` definitions 68 → 68 (no new enums; `reserved` is a marker), `config_example` allowlist unchanged.
- Roadmap: tasks.md **A.4** (plugin API contract — now wired to a loadable spec and versioned), **A.6** (one core, many front doors — the catalog is the shared front-door list), **P.198** (stable versioned plugin API), **P.206** (plugin manifest), **AV** (CI providers: reserved kinds made explicit). Design source: the 2026-08-22 extensibility audit plan (sections A0, C0, 2.7).
- No SQLite schema change. No render-path change (doctor and config validation are CLI; the app registry is startup-only).
