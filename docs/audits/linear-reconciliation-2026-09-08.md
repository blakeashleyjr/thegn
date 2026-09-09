# Thegn Linear Portfolio and Implementation Reconciliation

This is a read-only reconciliation of the Thegn Linear team with repository
`main` at `e616f489` on 2026-09-08. It covers all 29 non-completed issues, every
issue comment and recorded relationship, the team's project/initiative/cycle
inventory, implementation and test evidence, OpenSpec, and the earlier API,
plugin/UI, configuration, remote, packaging, and reliability audit findings.
No Linear record was changed.

> This document preserves the pre-mutation audit snapshot. Its recommendations
> were applied beginning 2026-09-08; use [`docs/PORTFOLIO.md`](../PORTFOLIO.md)
> and Linear for the live post-reconciliation state.

## Executive verdict

The repository is materially ahead of Linear. Linear is not currently a
trustworthy view of either delivered work or remaining scope.

- The team has 95 issues total and 29 open: 21 marked In Progress and 8 in
  Backlog. It has **zero projects, zero initiatives, and zero cycles**. All 29
  open issues are unprojected and unlabeled.
- Of the 29 open issues, **14 are likely delivered or stale**, **10 are partial
  or have diverged from their written scope**, and only **5 are unambiguously
  outstanding as written**. This is an implementation assessment, not a
  recommendation to erase intentionally deferred product ambitions.
- The five clear open items are THE-95 (migration authority fail-open), THE-90
  (sandboxed sccache), THE-93 (persistent flat-mode indicator), THE-92
  (network-interface icon semantics), and THE-41 (remote audit umbrella).
  Runtime portions of THE-94 and the compatibility architecture in THE-96 also
  remain real work, despite partial mitigation.
- Eight open issues are unassigned: THE-90 through THE-96, plus THE-69.
  Twenty-three have no priority. Six have empty descriptions, nineteen
  have no relations, and acceptance criteria usually live in a planning comment
  or an OpenSpec directory rather than in Linear.
- The API has an unusually good architectural foundation—a single capability
  catalog, stable scopes/errors, generated schema, gap ratchets, and multiple
  transports—but it is not feature-equivalent across HTTP, gRPC, MCP, CLI, and
  plugins. A thin client can do useful work; it cannot yet reproduce the native
  application.
- The UI is **not fully pluggable or arbitrarily editable through plugins**.
  Runtime plugins can contribute statusbar segments, notification sources,
  palette actions, and issue providers. Panels, sidebar tabs, themes, layout,
  chrome, keymaps, hit targets, drawers, overlays, and top-level apps remain
  native or narrowly config-driven.
- The config system is one of the strongest subsystems: typed models, strict
  validation, layered provenance, trust clamps, generated references, and hot
  reload. Its risks are size and lifecycle complexity: about 29,500 lines of
  config code, tolerant runtime fallback, stale docs/specs, and the schema
  migration failures captured by THE-94/95/96.
- OpenSpec is also not synchronized with delivery. There are 113 active change
  directories; 102 still contain unchecked tasks. The strict validator currently
  reports 169 passes and one failure, while `tasks.md` says everything is green.

## Portfolio state

| Signal                          | Current state | Interpretation                                                                                  |
| ------------------------------- | ------------: | ----------------------------------------------------------------------------------------------- |
| Total team issues               |            95 | Historical work is recorded, but the active view needs reconciliation.                          |
| Open issues                     |            29 | 21 In Progress, 8 Backlog.                                                                      |
| Likely delivered/stale          |            14 | Close after attaching merge/spec evidence and resolving deliberate scope decisions.             |
| Partial/scope-diverged          |            10 | Rewrite around the remaining outcome or close the delivered slice and file a bounded follow-up. |
| Clearly outstanding             |             5 | Keep open, add owners and acceptance criteria.                                                  |
| Unassigned                      |             8 | Includes every new reliability incident and THE-92/THE-69.                                      |
| No priority                     |            23 | Only THE-91/95 are Urgent, THE-90/94/96 High, and THE-93 Medium.                                |
| Projects / initiatives / cycles |     0 / 0 / 0 | There is no portfolio layer above individual issues.                                            |
| Issues with no relations        |            19 | Real dependencies are mostly implicit in prose and code.                                        |

Classification rules used here:

- **Delivered/stale** means the accepted or architect-approved scope is on
  `main`; remaining work is bookkeeping, a separately scoped follow-up, or an
  intentionally rejected ambition.
- **Partial/scope-diverged** means useful implementation shipped, but either the
  original Linear promise remains broader or the current OpenSpec contradicts
  what shipped.
- **Outstanding** means the reported behavior or audit work remains reproducible
  in current source.

## Complete open-issue ledger

Every issue below has no project, initiative, cycle, milestone, or label.

| Issue                                                 | Linear                                 | Assessment                               | Evidence and overlap                                                                                                                                                                                                                                                                                                                                      | Recommended Linear action                                                                                                                                                       |
| ----------------------------------------------------- | -------------------------------------- | ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [THE-95](https://linear.app/blakeashley/issue/THE-95) | Backlog · Urgent · unassigned          | **Outstanding**                          | The canonical shared DB still becomes unrestricted when no migration runtime is installed ([policy](../../crates/thegn-core/src/db.rs#L471)). This is the upstream cause of THE-94 and the permissive inverse of THE-96.                                                                                                                                  | Assign immediately. Fail closed for the canonical DB while preserving unrestricted temporary DBs; add the missing regression test.                                              |
| [THE-96](https://linear.app/blakeashley/issue/THE-96) | Backlog · High · unassigned            | **Partial / title stale**                | DB open still uses one global schema requirement ([initialization](../../crates/thegn-core/src/db.rs#L500)); there is no per-operation compatibility. Current `land` continues when optional DB opens fail, but silently loses its remote-target guard and lifecycle filing ([land](../../crates/thegn-host/src/cmd/land.rs#L28)).                        | Keep, but retitle to per-operation schema requirements and compatible no-migrate access. Qualify the claim that `land` itself is blocked.                                       |
| [THE-94](https://linear.app/blakeashley/issue/THE-94) | Backlog · High · unassigned            | **Partial; runtime defect remains**      | Startup now preflights the DB, but periodic hydration still discards the error and constructs an unlabeled fallback model ([hydrate](../../crates/thegn-host/src/hydrate.rs#L3553)). THE-95 can still trigger the observed mid-run path.                                                                                                                  | Keep. Specify a typed sticky refusal state, preserved prior model, banner lifecycle, and runtime regression test.                                                               |
| [THE-90](https://linear.app/blakeashley/issue/THE-90) | Backlog · High · unassigned            | **Outstanding**                          | Cache env/mount support exists ([build cache](../../crates/thegn-host/src/build_cache.rs#L29)), but later pipeline evidence still clears `RUSTC_WRAPPER`; the dev shell independently injects sccache ([flake](../../flake.nix#L518)). No doctor probe or contained compile smoke exists.                                                                 | Assign. Make cache use explicit/fail-soft, unify dev-shell and config authority, add doctor output and a sandbox smoke test.                                                    |
| [THE-93](https://linear.app/blakeashley/issue/THE-93) | Backlog · Medium · unassigned          | **Outstanding**                          | Flat/grouped mode persists and `g` toggles it, but the frame/header carries no persistent flat flag or chip ([header](../../crates/thegn-host/src/sidebar_view.rs#L196)).                                                                                                                                                                                 | Keep; add the same persistent truthfulness treatment already used for sort mode.                                                                                                |
| [THE-92](https://linear.app/blakeashley/issue/THE-92) | Backlog · no priority · unassigned     | **Outstanding / underspecified**         | `[stats].net_icon` is a single fixed glyph and the renderer uses aggregate traffic ([config](../../crates/thegn-core/src/config.rs#L2800), [chrome](../../crates/thegn-host/src/chrome.rs#L1469)). Metrics retain names but not interface type/default route.                                                                                             | Define Ethernet/Wi-Fi/VPN precedence, cross-platform classification, fallback, and tests before implementation.                                                                 |
| [THE-41](https://linear.app/blakeashley/issue/THE-41) | Backlog · no priority · Blake          | **Outstanding umbrella**                 | No OpenSpec was opened. Plaintext serve remains explicit ([daemon config](../../crates/thegn-core/src/config_daemon.rs#L154)); local-admin relies on filesystem modes rather than peer credentials; route-to-host remains incomplete.                                                                                                                     | Replace with a remote epic and bounded children: TLS contract, local IPC identity, route-to-host completion, transport/provider audit, and an explicit product-parity decision. |
| [THE-91](https://linear.app/blakeashley/issue/THE-91) | Backlog · Urgent · unassigned          | **Delivered/stale**                      | Env precedence, relocated home mounts, and writable worktree/git-common mounts shipped ([agent](../../crates/thegn-host/src/agent.rs#L3427), [sandbox](../../crates/thegn-core/src/sandbox.rs#L738)); the final Linear comment records an end-to-end contained worker commit.                                                                             | Close. File doctor/automated end-to-end enforcement as a separate reliability issue.                                                                                            |
| [THE-22](https://linear.app/blakeashley/issue/THE-22) | In Progress · no priority · Blake      | **Delivered to accepted scope**          | Durable per-thread watched-PR tasks and explicit `h` admission shipped; the final design deliberately rejects automatic dispatch ([proposal](../../openspec/changes/add-watched-pr-comment-tasks/proposal.md#L22), [core](../../crates/thegn-core/src/pr_review_tasks.rs#L136)). Depends on THE-27, though Linear does not say so.                        | Close if final OpenSpec is authoritative. Otherwise rewrite only the policy-controlled auto-admission remainder; fix stale schema-version prose.                                |
| [THE-27](https://linear.app/blakeashley/issue/THE-27) | In Progress · no priority · Blake      | **Delivered/stale**                      | Snapshot/anchoring, resolved filtering, diff rendering, bounded agent handoff, and confirmation all shipped ([review model](../../crates/thegn-core/src/review.rs#L11), [view](../../crates/thegn-host/src/pr_view.rs#L334)).                                                                                                                             | Close and archive/check off OpenSpec; link it as THE-22's dependency. Track literal-glyph cleanup separately if still desired.                                                  |
| [THE-15](https://linear.app/blakeashley/issue/THE-15) | In Progress · no priority · Blake      | **Partial**                              | The Nix batteries distribution pins Alacritty/font/config ([batteries](../../nix/batteries.nix#L1)), but the installer has no batteries mode and macOS/Windows/native bundle fidelity is absent. Strongly overlaps THE-52.                                                                                                                                | Keep, replace the empty description, and split by platform plus clean-host rehearsal. Share one platform matrix with THE-52.                                                    |
| [THE-52](https://linear.app/blakeashley/issue/THE-52) | In Progress · no priority · Blake      | **Partial**                              | Artifacts, attestations, Homebrew/AUR rendering, deb/rpm build, and staging exist ([release matrix](../../packaging/release.json#L1)); external publication rehearsal and several channels remain deferred ([release guide](../../RELEASING.md#L80)).                                                                                                     | Rename from “every possible package manager”; create bounded channel/rehearsal children and connect it to THE-15.                                                               |
| [THE-21](https://linear.app/blakeashley/issue/THE-21) | In Progress · no priority · Blake      | **Partial / major spec drift**           | A strong typed-event/catalog-action runtime shipped ([automation](../../crates/thegn-core/src/automation.rs#L17)), but current OpenSpec still requires schedule/session triggers and operator run/enable/audit surfaces that do not exist ([spec](../../openspec/changes/add-automation-rules/specs/automations/spec.md#L5)). Overlaps THE-19 and THE-62. | Decide whether shipped typed facts supersede the old spec. Rewrite and close, or keep only explicitly chosen missing triggers/operator controls.                                |
| [THE-51](https://linear.app/blakeashley/issue/THE-51) | In Progress · no priority · Blake      | **Partial**                              | Fluent, locale fallback, en-US/ja-JP, parity and pseudo-locale tooling landed, but the production adapter is intentionally bounded to statusbar/palette ([adapter](../../crates/thegn-host/src/i18n_surface.rs#L1)); active tasks remain 0/17.                                                                                                            | Keep but replace “Full localization” with milestone slices: chrome, time/age, help, RTL, CLI invariance, and plugin-string contract.                                            |
| [THE-13](https://linear.app/blakeashley/issue/THE-13) | In Progress · no priority · Blake      | **Partial versus title**                 | A safe local preview loop shipped ([preview](../../crates/thegn-core/src/config_preview.rs#L12)); browser product/import/DOM automation was rejected. Public `browser.drive` is still advertised but always unimplemented.                                                                                                                                | Close the delivered preview-loop slice; create separately prioritized browser automation/product issues only if still desired. Remove or truthfully classify the stub.          |
| [THE-17](https://linear.app/blakeashley/issue/THE-17) | In Progress · no priority · Blake      | **Partial versus title**                 | Configurable external-editor handoff from UI/control/MCP shipped ([handoff](../../crates/thegn-core/src/ide_handoff.rs#L18)); `thegn://`, desktop registration, extensions, and inbound integration were cut.                                                                                                                                             | Close/rename the handoff slice and file only selected deeper integrations with packaging/client acceptance criteria.                                                            |
| [THE-34](https://linear.app/blakeashley/issue/THE-34) | In Progress · no priority · Blake      | **Partial / API defect remains**         | Stable error vocabulary and filtered HTTP/WS/SSE event feeds shipped. Yet generic feed filters accept `snapshot`/`delta` while pane bytes only travel attach streams ([wire](../../crates/thegn-core/src/control_wire.rs#L167), [HTTP](../../crates/thegn-svc/src/control/http.rs#L1638)).                                                                | Close the delivered error/filter slice, then file the truthful bootstrap/filter contract needed by THE-40 clients.                                                              |
| [THE-26](https://linear.app/blakeashley/issue/THE-26) | In Progress · no priority · Blake      | **Partial versus planning scope**        | The audit and a BugStalker-only Linux/x86-64 path shipped ([debug](../../crates/thegn-core/src/debug.rs#L1)); DAP, gdb/lldb, stepping/variables, adapter config, and a debugger API/provider remain absent ([help](../help/debugging.md#L83)).                                                                                                            | Close the audit as a decision artifact; create separate adapter/DAP/product issues if chosen.                                                                                   |
| [THE-49](https://linear.app/blakeashley/issue/THE-49) | In Progress · no priority · Blake      | **Delivered/stale decision**             | The accepted resolution is “memory is an MCP preset, not a native feature” ([proposal](../../openspec/changes/add-mcp-proxy-hub/proposal.md#L24)); proxy partitions, policy, presets, and credential custody shipped. Child of completed THE-16.                                                                                                          | Close. Track proxy-daemon reload/list-changed residuals under THE-16 or a new implementation issue; remove the operational THE-90 relation.                                     |
| [THE-62](https://linear.app/blakeashley/issue/THE-62) | In Progress · no priority · Blake      | **Delivered/stale**                      | Named ntfy/webhook/Discord/Slack outbound sinks, secret refs, routing, rate limits, bounded fan-out, and docs shipped ([config](../../crates/thegn-core/src/config_push.rs#L25), [routing](../../crates/thegn-core/src/notification_route.rs#L92)).                                                                                                       | Close after OpenSpec reconciliation. Inbound bots are a different product and need a new issue.                                                                                 |
| [THE-20](https://linear.app/blakeashley/issue/THE-20) | In Progress · no priority · Blake      | **Product delivered; formal spec stale** | Safe per-worktree embedded skill seeding for Claude/Codex/Pi shipped ([skills](../../crates/thegn-core/src/skills.rs#L1), [docs](../help/skills.md#L9)); the proposal still describes vendor-home `sync` and a different bundle.                                                                                                                          | Replace the obsolete spec with the safer shipped design, then close. Keep distinct from THE-49's MCP resources.                                                                 |
| [THE-60](https://linear.app/blakeashley/issue/THE-60) | In Progress · no priority · Blake      | **Delivered/stale**                      | Detection, modes, cache identity, trust requests, fill-only env composition, explicit install, prewarm, doctor, and docs shipped ([activation](../../crates/thegn-core/src/toolchain_activation.rs#L226), [provider](../../crates/thegn-host/src/mise_provider.rs#L197)).                                                                                 | Close and reconcile the wholly unchecked task file. Link trust/env overlap with THE-19.                                                                                         |
| [THE-32](https://linear.app/blakeashley/issue/THE-32) | In Progress · no priority · Blake      | **Delivered/stale**                      | Strict `.gitmodules`, escape rejection, pointer rendering, atomic patches, no partial line staging, trust-gated init, UI, and tests shipped ([core](../../crates/thegn-core/src/submodule.rs#L41), [service](../../crates/thegn-svc/src/git/submodule.rs#L19)).                                                                                           | Close and reconcile OpenSpec. Link remote-git implications to THE-41.                                                                                                           |
| [THE-55](https://linear.app/blakeashley/issue/THE-55) | In Progress · no priority · Blake      | **Delivered/stale**                      | Host-local fenced migration, live refusal/kill, sanitized import/readback, collision/rollback, smoke coverage, and docs shipped ([move](../../crates/thegn-host/src/cmd/session_move.rs#L1), [carrier](../../crates/thegn-core/src/session_migration.rs#L142)).                                                                                           | Close after normal main-gate confirmation.                                                                                                                                      |
| [THE-19](https://linear.app/blakeashley/issue/THE-19) | In Progress · no priority · Blake      | **Delivered/stale**                      | Six lifecycle events, layered/trust-gated resolution, bounded process groups/output, all call sites, smoke tests, and docs shipped ([model](../../crates/thegn-core/src/hooks.rs#L11), [runner](../../crates/thegn-host/src/hook_run.rs#L1)).                                                                                                             | Close and update the stale task checklist. Link event overlap to THE-21 and environment/trust overlap to THE-60.                                                                |
| [THE-40](https://linear.app/blakeashley/issue/THE-40) | In Progress · no priority · Blake      | **Delivered decision; no GUI**           | The accepted result explicitly says not to build a GUI and defines a future thin-client lane ([decision](../superpowers/specs/2026-08-29-native-gui-frontend-lane-design.md#L1)). Required binary frame/reconnect/layout contracts do not yet exist.                                                                                                      | Close as a “not now” architecture decision. File THE-40-F1 only when its published observer-client contract is prioritized.                                                     |
| [THE-69](https://linear.app/blakeashley/issue/THE-69) | In Progress · no priority · unassigned | **Delivered/stale audit**                | The Aura audit selected one bounded adoption: shared model-proxy budget-cap notification, with sanitization and tests ([policy](../../crates/thegn-core/src/budget_alert.rs#L39), [host](../../crates/thegn-host/src/usage_budget.rs#L16)).                                                                                                               | Close; file selected provenance/session-attribution follow-ups rather than keeping the idea-audit open.                                                                         |
| [THE-7](https://linear.app/blakeashley/issue/THE-7)   | In Progress · no priority · Blake      | **Delivered/stale v1**                   | Modal draft/live preview, file-backed themes, watcher, CLI, Gogh import, contrast, docs, and tests shipped. Base16/export were explicitly deferred ([design](../../openspec/changes/add-theme-builder-overlay/design.md#L105)).                                                                                                                           | Close the accepted v1 and reconcile 0/24 tasks. A runtime `Theme` plugin is separate missing UI-platform work.                                                                  |
| [THE-11](https://linear.app/blakeashley/issue/THE-11) | In Progress · no priority · Blake      | **Delivered/stale**                      | Ordered arbitrary terminal drawer tools reuse `[[tools]]`, with scopes, validation, containment, selection, pooling, indicator, examples, and docs ([config](../../crates/thegn-core/src/config_drawer.rs#L16), [help](../help/drawer-and-corner.md#L40)).                                                                                                | Close; only generic e2e/full-CI boxes remain unchecked. Do not describe it as arbitrary UI plugin support.                                                                      |

## How the issue clusters overlap

### Schema safety is one architecture program

THE-95, THE-96, and THE-94 are complementary layers of the same incident, not
duplicates:

1. THE-95 is the authority hole: a process without installed policy can mutate
   the canonical database.
2. THE-96 is the compatibility hole: policy-aware clients cannot declare the
   older schema capabilities an operation actually needs.
3. THE-94 is the truthfulness hole: a live UI can turn a refusal into apparently
   missing data.

The order should be THE-95 invariant/regression first, then the THE-96 capability
model, with THE-94's typed UI state designed against the same error vocabulary.
These belong in one reliability project with explicit causal links.

### Sandbox viability splits cleanly

THE-91's repository/git-write blocker is fixed and manually verified. THE-90 is
the still-open compiler-cache and diagnostics half. Their relationship should
remain historical, not imply that THE-91 blocks THE-90. A separate small
follow-up can enforce the “Codex full access only inside Thegn bwrap” operational
pairing and add an end-to-end worker-commit probe.

### PR review is a dependency chain

THE-27 owns review snapshots, rendering, and manual handoff. THE-22 consumes it
to create durable watched-thread tasks. Linear encodes neither dependency. The
implementation incident relation from THE-22 to THE-91 is no longer a product
dependency. Close both delivered scopes, link THE-27 → THE-22, and decide
separately whether automatic admission is a product goal.

### Packaging needs one platform matrix

THE-52 owns reproducible artifacts, attestations, package metadata, and
publication channels. THE-15 owns a complete terminal/font/config/runtime
experience on each platform. The present issues overlap heavily but do not have
a relation. One project should use a matrix with rows for Linux GNU/musl, Nix,
macOS, Windows, Homebrew, AUR, deb/rpm repositories, Scoop/winget, clean-host
rehearsal, and actual publication evidence.

### Automation is a three-stage flow

THE-19 produces trusted lifecycle events; THE-21 evaluates typed rules and calls
catalog actions; THE-62 delivers `notify.push` through external sinks. Their
security boundaries—repo trust, secret references, bounded execution, loop
suppression, and rate limiting—are cohesive. Linear currently presents them as
unrelated individual ideas.

### API, browser, IDE, GUI, and remote are one client-platform question

THE-34 supplies errors and event filtering. THE-13 and THE-17 consume public
capabilities. THE-40 is the explicit future native-client decision. THE-41 owns
remote transport/confidentiality questions. Closing their delivered slices is
reasonable, but their remaining work should be organized under a client-platform
project rather than kept as vague “full integration” tickets.

### Runtime extensibility is not represented by an open issue

THE-7 makes theme values editable and THE-11 makes drawer terminal occupants
configurable. Neither makes UI structure pluggable. The broad UI-contract ticket
[THE-43](https://linear.app/blakeashley/issue/THE-43) is Done even though its own
OpenSpec marks phases 2–5 deferred, including placement grammar and the
`PanelSection` runtime ([tasks](../../openspec/changes/add-ui-component-contract/tasks.md#L38)).
This is the largest direct mismatch between the plugin audit and Linear.

## API deep dive

### What is good

- `thegn_core::capability::CATALOG` is a real single source for capability ids,
  surface projection, scopes, and stub/excuse metadata
  ([catalog](../../crates/thegn-core/src/capability.rs#L200)).
- HTTP/WS/SSE, gRPC, CLI, MCP, and plugins share domain/control types instead of
  inventing unrelated APIs.
- The read/write/git/exec/admin scope lattice is explicit and tested; write,
  git, and exec remain mutually independent powers
  ([scopes](../../crates/thegn-core/src/control.rs#L27)).
- Closed error codes, request auditing, a generated JSON schema snapshot, and a
  shrink-only gap ratchet provide unusually honest evolution discipline
  ([schema](../api/control-v1.json), [gap ledger](../../test/surface-gaps-ratchet.txt)).

### What is incomplete or misleading

`thegn api coverage` currently reports:

| Surface | Implemented | Stub | Excused debt | Declared/intended |  Effective implementation |
| ------- | ----------: | ---: | -----------: | ----------------: | ------------------------: |
| HTTP    |          52 |    1 |           27 |                80 |                       65% |
| gRPC    |          32 |    1 |           39 |                72 |                       44% |
| CLI     |          55 |    1 |            4 |       60 intended |                       92% |
| MCP     |          19 |    0 |           28 |                47 |                       40% |
| Plugin  |          43 |    1 |            0 |                44 | 98% catalog-call coverage |

The CLI command prints 93 declared catalog rows, but only 60 are intended for
CLI; dividing 55 by 93 would be misleading. “Plugin 98%” is also only generic
host-call catalog coverage, not UI extension coverage.

Specific defects:

- `browser.drive` is published across surfaces and in the generated contract,
  yet the daemon always returns `Unimplemented`. This directly overlaps THE-13.
- The generic event API accepts `snapshot` and `delta` filters that cannot match
  there because pane bytes only ride session-attach streams. This overlaps
  THE-34 and blocks a truthful THE-40 observer-client contract.
- The control API design doc and plugin help omit the newer independent `exec`
  scope; plugin help also still shows API `0.2.0` while the wire is v0.3
  ([plugin help](../help/plugins.md#L9)).
- The future GUI decision identifies missing binary frame, reconnect/version
  skew, observer ownership, and serializable chrome/layout contracts. This API
  is suitable for automation and monitoring, not yet as the complete backend of
  an independently released native frontend.
- [THE-39](https://linear.app/blakeashley/issue/THE-39) is Done after raising
  control-surface coverage from 58% to 88%, but no open issue owns the remaining
  parity and contract-truthfulness work.

Verdict: the API is well-designed and unusually measurable, but incomplete. The
right next step is not “add endpoints until 100%” blindly; define supported
client classes, remove impossible/stub promises, and ratchet the exact surface
needed by each class.

## Plugin and UI deep dive

The native compositor is intentionally in-process. Its chrome module says there
is no WASM or plugin hop ([chrome](../../crates/thegn-host/src/chrome.rs#L1)). The
wire vocabulary advertises thirteen extension-point families, including panel,
sidebar, theme, automation, data, harness/program adapters, and tracker/CI/forge
providers ([wire](../../crates/thegn-core/src/plugin_api.rs#L168)). The runtime
host contract accepts only four:

| Runtime extension point            | Actually accepted   | User-visible result                                                                     |
| ---------------------------------- | ------------------- | --------------------------------------------------------------------------------------- |
| `StatusBarSegment`                 | Yes                 | Cached status segment                                                                   |
| `NotificationSource`               | Yes                 | Notification-center events                                                              |
| `PaletteAction`                    | Yes                 | Command-palette row/action                                                              |
| `IssueProvider`                    | Yes                 | Issue backend in the panel                                                              |
| `PanelSection`                     | No                  | Wire type exists; runtime/placement is deferred                                         |
| `SidebarTab`                       | No                  | Not negotiated                                                                          |
| `Theme`                            | No                  | Native file/config theme system only                                                    |
| `Automation`                       | No                  | Native typed automation system only                                                     |
| `DataSource`                       | Not in general host | A calendar-account-specific command adapter exists; it is not a general UI contribution |
| Harness/program/CI/forge providers | No                  | Reserved or native closed seams                                                         |

The authoritative runtime list is only four entries
([host contract](../../crates/thegn-svc/src/plugin/loader.rs#L13)). The
`PanelSection` wire documentation calls it the second wired surface even though
the host rejects it; that is an API-contract defect. Plugins run as NDJSON
subprocesses, get scoped/audited generic `host.call`, and resident plugins have
restart backoff, but plugin processes are explicitly **not sandboxed**
([security note](../help/plugins.md#L72)).

Therefore:

- A plugin cannot add or replace an arbitrary panel section, sidebar/tab,
  drawer, overlay, top-level application, chrome element, layout rule, keymap
  zone, or hit target.
- Config can reorder or customize selected native surfaces, theme values, tools,
  and bars, but there is no common plugin placement grammar for arbitrary chrome.
- `tg-kit::AppTile` is a compile-time embedded Rust application contract, not a
  runtime plugin surface.
- No locale contract was found for plugin-provided user-visible strings, which
  overlaps THE-51.
- No debugger provider extension exists, which overlaps THE-26 and THE-17.

Recommendation: reopen/split the remaining THE-43 program rather than claiming
full pluggability. Sequence it as (1) truthful v0.3 contract/docs, (2)
`PanelSection` negotiation/render/cache/placement, (3) common element ids and
placement grammar, (4) sidebar/theme/key-zone decisions, and (5) optional plugin
sandbox profiles.

## Configuration deep dive

The config architecture is mature:

```text
built-in defaults
  → trusted global TOML / --config
  → profile config.toml
  → THEGN_* value overlays
  → --set values/fragments
  + separately parsed, trust-clamped repo .thegn.{toml,yaml,yml,json}
```

The ordering and trust boundary are documented in
[configuration help](../help/configuration.md#L11) and
[architecture](../ARCHITECTURE.md#L215). Notable strengths are typed Rust models,
strict `config validate`, tolerant startup with diagnostics/defaults, `config
explain` provenance, typo suggestions, reserved-kind rejection, TOFU trust
resolution, generated schema/reference/example parity, Home Manager/env drift
tests, and hot reload.

Risks and current findings:

- `crates/thegn-core/src/config*.rs` totals about 29,512 lines; `config.rs` alone
  is 6,890. Breadth is excellent, but policy is dispersed enough that new
  precedence bugs are plausible.
- Runtime tolerant loading drops unknown keys with a warning while strict
  validation rejects them ([behavior](../help/configuration.md#L328)). This is a
  sensible availability policy only if warning/doctor visibility remains strong.
- The local user configuration used for this audit still contains deprecated
  `workspaces_dir` and `workspace.cms` spellings. The repo-local `.thegn.toml`
  validates; this is local hygiene rather than a repository defect.
- THE-90 demonstrates competing environment/config authority:
  `RUSTC_WRAPPER` comes from the dev shell even when modeled sccache config says
  otherwise.
- THE-94/95/96 show that database-schema policy is now a first-class config and
  lifecycle concern, not merely a migration implementation detail.
- The closed [THE-38](https://linear.app/blakeashley/issue/THE-38) config audit
  correctly delivered the major substrate, but no open issue owns continued
  config-complexity and provenance regression auditing.

Verdict: the config system is powerful, disciplined, and user-debuggable. Its
next maturity step is consolidation and invariant testing, not more surface area.

## OpenSpec, roadmap, and documentation drift

The repository says OpenSpec is authoritative, in-flight changes are proposals,
and implemented changes are archived before roadmap boxes become complete
([policy](../../tasks.md#L133)). Current practice violates that model:

- 113 active change directories remain; 102 have at least one unchecked task.
  Many correspond to merged features such as THE-7, THE-20, THE-27, THE-32,
  THE-60, and THE-62.
- `just openspec-validate` reports **169 passed, 1 failed**. The failing
  `add-tracker-provider-suite` change, associated with completed THE-50, lacks
  required delta headers and `#### Scenario:` sections.
- `tasks.md` still claims both `160/160` and `87/87` validation success
  ([sweep](../../tasks.md#L144), [capability index](../../tasks.md#L199)).
- THE-21 and THE-20 have particularly serious normative drift: their task/spec
  files describe designs intentionally replaced by safer shipped designs.
- THE-40 was archived, but its architecture-gate delta was not folded into the
  current base architecture-gates spec; the decision is prose, not a live
  normative gate.
- `KNOWN_ISSUES.md` says the remote daemon had no open issues at release time
  ([remote section](../../KNOWN_ISSUES.md#L48)); THE-41 and the September
  reliability incidents now make that statement stale.

This is not cosmetic. Reviewers cannot reliably tell whether an unchecked task
means missing code, rejected scope, missed bookkeeping, or an invalid spec.

## Recommended portfolio structure

Create projects only after the stale issue pass, so completed work does not
pollute active project health:

1. **Alpha Reliability and State Compatibility**
   - THE-95 → THE-96 → THE-94 causal chain
   - THE-90
   - THE-93 and THE-92 only if small UX correctness is part of this milestone
   - New: stage-worker containment doctor/smoke from closed THE-91
2. **Client API and Remote Access**
   - Recast THE-41 as the parent audit
   - New: API client-class contract and surface ratchet
   - New: remove/fix `browser.drive` and impossible event filters
   - Optional chosen follow-ups from THE-13/17/34/40
3. **Plugin UI Platform**
   - Remaining THE-43 phases, beginning with `PanelSection`
   - Plugin contract/docs version correction
   - Placement/sidebar/theme/localization decisions and sandbox posture
4. **Distribution and Release Readiness**
   - Re-scoped THE-15 and THE-52 children on a shared platform/channel matrix
5. **Spec and Tracker Reconciliation**
   - Close the 14 delivered tickets with evidence
   - Decide/rewrite the 10 partial scopes
   - Fix the failing THE-50 OpenSpec change
   - Archive or rewrite active merged changes and regenerate roadmap status

Do not use a project as a substitute for issue acceptance criteria. Every kept
issue should have an owner, priority, explicit “done when,” implementation/spec
links, and causal/dependency relations.

## Priority order

1. **THE-95**: fail-open migration authority can mutate shared live state and
   invalidate every policy-aware process's assumptions.
2. **THE-96 + THE-94**: introduce truthful per-operation compatibility and make
   refusals durable and visible in the running UI.
3. **THE-90**: restore trustworthy contained pipeline gates and remove
   environment/config ambiguity.
4. **Tracker/spec reconciliation**: close stale work before planning from Linear;
   otherwise project metrics will be knowingly false.
5. **THE-41 decomposition and API contract fixes**: security/confidentiality and
   remote/client promises need bounded ownership.
6. **THE-15/THE-52**: turn partial artifact machinery into tested distribution
   outcomes.
7. **Plugin UI platform**: only if runtime UI extensibility is a product promise;
   today it is not delivered.
8. **THE-93/THE-92 and selected “deep/full” follow-ups**: small UX work and
   optional product expansion after their semantics are specified.

## Verification and sources

Primary sources:

- The connected [Thegn Linear team](https://linear.app/blakeashley/team/THE),
  including all 29 open issue records, comments, relations, statuses, priorities,
  assignments, and project/initiative/cycle listings.
- Repository `main` at `e616f489`, especially
  [architecture](../ARCHITECTURE.md), [roadmap](../../tasks.md),
  [known issues](../../KNOWN_ISSUES.md), [OpenSpec](../../openspec), generated
  [control schema](../api/control-v1.json), and the linked implementation/tests
  above.

Commands run during the audit:

- Focused config/plugin/capability/migration/control-schema tests: **855 passed,
  0 failed**.
- `target/debug/thegn api coverage`: values reproduced in the API table.
- `target/debug/thegn plugin list`: no plugins configured in this audit profile;
  runtime capability conclusions come from the host contract, not this empty
  local list.
- `target/debug/thegn config validate`: repo overlay valid; two deprecated keys
  found in the auditor's user-global config.
- `just openspec-validate`: **169 passed, 1 failed**.

The working tree was clean before this report. The report is the only intended
repository change from the reconciliation.
