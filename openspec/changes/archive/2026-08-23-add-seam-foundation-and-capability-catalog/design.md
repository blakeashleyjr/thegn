## Context

thegn has ~30 provider seams in three idioms (trait + hand-written enum router; trait + `dyn`; plain enum + `match`), three external surfaces (control HTTP/gRPC, MCP, plugin `HostVerb`s) each with its own verb list, and a v0.1 plugin wire that cannot express a reply. The 2026-08-22 audit plan converges all of this in phases; this change is phase 0: the shared foundation that later phases (forge unification, `[git] backend`, editor seam, plugin runtime, provider-as-plugin) build on. Nothing existing is migrated onto the new types here beyond what is needed to prove them (doctor, routes table, app registry, reserved kinds).

Constraints: `thegn-core` stays tokio/termwiz-free and 95% line-covered; the event loop's 0%-idle contract is untouched (nothing here runs on the render loop — doctor, validate, and the route table are CLI/startup); no SQLite schema change.

## Goals / Non-Goals

**Goals:**

- One vocabulary (`seam`) later seams can adopt without redesign; `Ladder`/`Router` prove it with unit tests.
- `kind ⇔ implemented-or-reserved` becomes a macro-emitted fact, not a comment.
- One capability list with per-surface coverage tests; HTTP routes built from it; the one known gap (`ListWorktrees`) closed.
- Plugin wire v0.2 shapes landed and pinned by a schema snapshot, so the runtime phase is additive.

**Non-Goals:**

- Migrating `IssueBackend`/`CiProvider`/forge onto `Ladder`/`Router` (later phases).
- A plugin loader/runtime, MCP state tools, `thegn api` CLI (later phases).
- Implementing Drone/Woodpecker/Jenkins/Argo/Forgejo/Gitea/Jellyfin/WSL — they become `reserved`.
- The architecture ratchets (platform-cfg, forge-leak, etc.) — the next change.

## Decisions

- **`seam` lives in core, glue in svc.** Core gets only std+serde types (`BoxFuture` is a `Pin<Box<dyn Future + Send>>` alias — no `futures` dep). `blocking()` (spawn_blocking), `Ladder`, `Router`, `registry` go in `thegn-svc/src/seam/`. Alternative: everything in svc — rejected because later pure seams (`editor`) and `config_enum!` need `Kind`/`Probe` from core.
- **Object-safe `BoxFuture` traits, never `async fn` in trait.** Mirrors `ControlApi`, the one seam that already has HTTP/gRPC/CLI adapters over it. Enum routers are retired seam-by-seam later; here only the generic `Ladder<dyn T>`/`Router<dyn T>` are built, and their per-seam `impl T for Ladder<dyn T>` forwarding is written once per seam when that seam migrates.
- **`reserved` is a macro token, not a separate list.** `config_enum!` grows an optional `reserved` suffix per variant. It emits `impl Kind`, puts `"reserved": [...]` in `x-thegn-enum`, and `from_str_validated` returns `Err("… is reserved: accepted but not implemented in this build")`. The strict walker (`config_validate::check_enum`) needs no change beyond reading the marker. Lenient deserialize keeps warn-and-default so a user's config never blocks launch. Pin count stays 68.
- **Dead CI sub-tables are removed, not kept.** `DroneCiConfig` etc. are config surface with nothing behind them; the `config_example` schema-walk test proves the example file stops documenting them.
- **Catalog keys on `Verb`.** `required_scope(verb)` remains the single policy; `HostCapability.verb` points at it. `Verb::ALL` is a hand-maintained const with an exhaustiveness test (no `strum` in the workspace). New verbs `PrStatus` (Read) and `NotifyPush` (Write) are added now so the catalog can list them; their HTTP/MCP implementations are excused in `SURFACE_GAPS` until the client-API phase.
- **`ROUTES` table in svc.** `(Method, path, CapId, handler)` — the handler fn pointer lives with the table so `router()` is a fold. Coverage tests: `http_routes_cover_catalog`, `grpc_methods_cover_catalog` (feature-gated `control-grpc`, seeded gaps for merge/calendar/pairings/wait/split which gRPC lacks today), `cli_verbs_cover_catalog`, `mcp_tools_cover_catalog` (today: all state caps gapped; docs tools are not catalog items), `plugin_host_calls_cover_catalog` (today: all gapped). Each test is bidirectional (stale gap ⇒ fail).
- **`GET /v1/worktrees`** → `ControlApi::list_worktrees` (default `Unimplemented` like the calendar verbs) implemented by `DaemonService` via `WorkspaceStore::worktrees()` on `spawn_blocking`, returning a `WorktreeInfo { path, branch, repo_root, location }` wire type (not the DB row). gRPC mirror deferred to the client-API phase (gap entry).
- **`APP_BUILDERS` static slice**, not `inventory`/`linkme`: all tiles are in one crate, link-section magic is the one thing that breaks on the never-green macOS/Windows legs, and a slice is greppable.
- **Plugin wire v0.2 is additive.** `Frame` is `#[serde(untagged)]` over `RpcMessage` | `RpcResponse`; `RpcMessage` keeps `params` defaulted so v0.1 lines still parse. `PluginSpec` flattens `PluginManifest`. `Config.plugins` changes type — the only consumer is a config test fixture. The schema snapshot test uses `schemars` (already a core dep), writes `docs/api/plugin-api-0.2.json`, and is regenerated with `THEGN_UPDATE_SNAPSHOTS=1` (same idiom as `just help-ratchet-update`). `plugin_api` leaves `cov_ignore` because everything in it is pure.
- **Render/event-loop impact: none.** No new wake source; doctor/validate are subcommands; `AppHost::from_config` already runs at startup.
- **Help context:** no new interactive surface. `docs/help/configuration.md` gains a sentence on reserved kinds; `docs/help/daemon-and-sessions.md` lists `/v1/worktrees`.

## Risks / Trade-offs

- [Marking kinds reserved breaks strict validation for anyone who set `[ci] provider = "drone"`] → pre-alpha; lenient load still works; the message names the kind and says "reserved".
- [`Config.plugins` type change is a config-schema change] → no user-facing docs exist for `[[plugins]]` yet; the example-doc allowlist entry is kept this phase and removed when the loader lands.
- [Catalog tables drift from handlers] → the bidirectional coverage tests are the point; a new route without a catalog row fails `just test`.
- [Seeding `SURFACE_GAPS` with today's gRPC/MCP/plugin debt reads as "covered"] → the gap list is printed by a test as a count and listed in `docs/superpowers/specs/control-api.md`; it is shrink-only.
- [`Ladder`/`Router` land with no production caller] → their unit tests plus the doctor registry (which uses `Probe`) keep them exercised; the next phase (forge) is their first consumer.

## Migration Plan

Additive; lands in one PR. Rollback = revert. Config users naming a reserved kind see a strict-validate failure and a load-time warning. `docs/api/plugin-api-0.2.json` is committed with the change.

## Open Questions

- None blocking. Whether `NotifyPush`'s HTTP route lands here or in the client-API phase is a scope choice; it is gapped here to keep the change bounded.
