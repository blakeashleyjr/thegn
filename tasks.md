# superzej — roadmap & progress

661 features across 46 groups (A–AT). The list is really **two tracks joined by
one keystone**: an AI-free _shell_ track and an AI track, bridged by the **proxy**.
That shape drives the phasing below. The proxy is not just a router — it is the
**AI control plane**: the single interception point every agent's model traffic
crosses, so any cross-cutting concern is configured **once and inherited by every
harness**. The proxy track (**U/V/W**) graduates into a full **AI gateway / context
fabric** in **AR (541–586)**. Original numbering is preserved; gaps are deliberate
cuts (499, 500, 502, 505, 506, 507, 510 dropped from the moonshot set; web
dashboard 510 and voice 499 already cut; deadbranch stale-branch cleanup imported
as 659–671 under Y; brows-style release management imported as 672–683 under AT).
The dropped eval harness (505/506)
resurfaces, scoped, as the gateway's eval hooks (**AR 581**).

**Status legend:** `[x]` done · `[~]` in progress / partial · `[ ]` not started.
Per-feature statuses below are verified against the current codebase. See
`CLAUDE.md` for architecture.

---

## Progress summary (as of 2026-06-22)

**Where we are:** **Phase 1** (the AI-free shell) is largely complete — native git
management, the notification/event bus, and IDE panels (problems/tasks/tests/symbols)
have landed. **Phase 2's substrate** (sandbox + remote) and, more recently, **the
proxy** (groups U/V/W) are in. The agent layer (Q–T) and MCP (AL) remain unstarted —
still by design; the proxy was built and validated standalone, and is the next major
track's prerequisite.

**Shipped & solid:**

- **Shell core** — the native `superzej-host` compositor (termwiz + portable-pty +
  `CenterTree`); in-process chrome (sidebar/panel/tabbar/statusbar), workspaces (repos)
  - worktrees as tabs, session detach/attach/resurrection. (Zellij/WASM was stripped in
    Phase 0.)
- **Keybinds** — full registry, KDL splice, conflict detection, cheatsheet feed (`F`).
- **Config** — declarative TOML, layering, env/flag overlays, live reload, validation,
  95%-gated core.
- **Palette** — native iocraft Cmd-K, nucleo fuzzy + embedded ripgrep, file open.
- **Git** — complete native git management (stage/commit, branch, log/graph, blame,
  stash, merge/rebase sequencer, conflict UI, cherry-pick/revert) + per-worktree diff;
  **GitHub** PR panel (status/checks/review/create/merge/approve/rerun) via `gh`;
  lazygit as the fallback.
- **Files/editor/monitor** — yazi bottom drawer, fuzzy finder + ripgrep search,
  `$EDITOR` tool, embedded system/GPU monitors, tabbar stats widget.
- **Activity dots** — host-side `none→active→quiet→acked` state machine (`activity`).
- **Sandbox + remote (Phase 2 substrate)** — per-worktree podman/docker/bwrap/none
  backends, bind-mount-at-real-path, remote worktrees over ssh/mosh.
- **Notification/event bus** — first-class `EventBus` in core (PR/agent/test/log/
  worktree/process events), urgency thresholds, desktop notifications (`notify-send`),
  a notifications panel/inbox with Enter-to-expand, sidebar badges.
- **Proxy (AI substrate, Phase 2)** — `superzej-proxy` crate: dual-protocol relay
  (Anthropic SSE + OpenAI), ordered failover + load-balanced/speculative routing,
  limit-exhaustion/reset tracking, per-scope budgets + spend attribution, native
  in-flight token reduction; host auto-launches it.
- **IDE panels** — problems/diagnostics, task registry + test discovery, document
  symbols, and an LSP substrate (hover/signature/code-action preview, item 532).

**Notable remaining gaps (candidate next work):**

- **Agent layer (Q–T) + MCP (AL)** — now **underway via an embedded first-party
  harness**, not external-adapter-first. The harness is **`termite-agent`** (the
  `apps/termite-agent` submodule, its own `docs/ROADMAP.md`, currently through its
  Phase 4 autonomous-coding MVP). It is hosted as the **`agent` app tab**
  (`superzej-host/src/apps/agent.rs`, an `sz_kit::AppTile` driving termite-core's
  `AgentRuntime` on `spawn_blocking`). This makes group **R**'s ACP/native-adapter
  path **secondary** — superzej runs its own harness first; foreign-harness adapters
  are additive later. Substrate-first sequencing **all landed**: embedding seam →
  proxy as model path (per-worktree scoped virtual keys, revoked on teardown) →
  sandbox/policy boundary (`SandboxTerminalTool` via `enter_argv`) → notifications
  (`AgentDone`/`AgentFailed`) + live proxy-spend observability. See
  `docs/superpowers/specs/2026-06-22-embedded-agent-integration-design.md`.
- **AI. Notification bus polish** — bus, desktop notifications, and inbox are live;
  420 is a fixed event→notification mapping + urgency thresholds, not yet a
  user-configurable action-rules engine. Still missing: DND/quiet hours (426),
  per-profile routing (427), sound/bell (429), push-to-phone (422/423).
- **IDE Tier 1 tail** — Search Everywhere aggregation (523) and the agent-side of
  attention routing remain; git, problems, tasks/tests, and symbols panels have landed.
- **B.** badge counts per row (28).
- **Orca-audit adds (654–658)** — per-line AI/human attribution overlay (654), agent-writable
  worktree status field (655), per-agent account hot-swap chip (656), agent-hook passthrough +
  worktree setup hooks (657), and agent session history/hibernation (658); plus enriched
  scheduled automations (226/504) and worktree setup hooks (54). Captured from an Orca feature audit.
- **deadbranch import (659–671)** — safe stale-_branch_ cleanup extending branch
  management (323): age-threshold detection (659), merge-aware delete (660),
  protected/WIP exclusion (661), delete-with-backup + restore (662/663), dry-run
  (664), local+remote scope (665), health stats (666), and a multi-select cleanup
  TUI (667); plus personal filter (668), PR-aware staleness (669), JSON/CSV export
  (670), per-repo config (671). Distinct from worktree GC (48/56). Captured from a
  deadbranch feature import.
- **brows release import (672–683)** — cross-forge **release & tag** management, the
  surface the Z/AT groups were missing: a release browser + detail view (673/674,
  the brows browsing UX generalized), tag management (675), create/edit/delete +
  draft/prerelease/latest flags (676/677), assets (678), auto-generated/AI notes
  (679), version diff/changelog (680), worktree/PR linkage (681), release
  notifications (682), per-forge config (683). AI-free except the notes narrative.
  Captured from a brows feature import.

**✔ E. Pinned programs / tiles — complete on the native host (items 57–74).** The
full config-driven pin/daemon system ships in `superzej-host`: a `PinSupervisor`
(`crates/superzej-host/src/pins.rs`) owning daemon panes across tab/workspace
switches, a real top-strip chrome region + tabbar chips, eager/lazy start,
restart-on-exit + health, singleton dedupe, promote/unpin at runtime, per-program
env, and resurrect via `session_state.pin_state`. See §E below for the per-item map.

---

## The dependency spine

```
L0  Foundation (daemon, zellij, event bus, state, config)
        │
        ├──────────────► L1  Workspace shell  ──► AI-FREE PRODUCT (511–515)
        │                     (bar, worktrees, pins, keybinds, sessions)
        │                          │
        │                          ├──► L2  Shell enrichment (git, files, editor,
        │                          │         palette, theming, notifs, monitor, remote)
        │                          │
        ├──► L3  Containers  ──────┤
        │       (sandbox, net, observ.)
        │                          │
        └──► L4  Proxy ────────────┤◄── the KEYSTONE for everything AI
                + cost/limits      │    (graduates into the AI gateway /
                + brokerage        │     context fabric — AR. configure once,
                + gateway (AR)     ▼     every harness inherits it)
                            L5  Agent layer (orchestration → ACP/adapters
                                 → observability → review/merge)
                                      │
                  ┌───────────────────┼───────────────────┐
                  ▼                   ▼                    ▼
            L6 API + MCP        L7 sem/weave         L8 GitHub/Linear
                                 (upgrades review)
```

Profiles, the event bus, and the audit log are cross-cutting and must be seeded
early even though most consumers arrive late — retrofitting observability hurts.
The profile/subprofile firewall is now designed end-to-end (see H. and
`docs/superpowers/specs/2026-06-11-profiles-subprofiles-design.md`).

---

## Phased roadmap (topologically ordered)

### Phase 0 — Foundation · P0 · blocks everything

Groups **A (1–12)** + config core **O (185–189)**. Daemon, zellij substrate, plugin
host, IPC, **event bus (9)**, state store (10), declarative config + layering +
reload. No user-facing surface; pure substrate. Seed the **central audit log (481)**
here so all later events are captured from day one.

### Phase 1 — AI-free workspace shell · P0 · **first shippable product** ◀ CURRENT

This _is_ the AI-free mode (511–515) — not a later toggle, the MVP. The discipline:
every feature here must **not hard-depend on AI**.

- Shell: **B (13–25), C (29–36), D (41–49), E (57–66, 73), F (75–83), G (89–92), I (111–113)**
- Cheap, high-value enrichment: basic git **Y (319–327)**, files **AF (395–398, 401)**,
  editor handoff **AG (405–406, 408–409)**, palette **M (161–166)**, theming
  **N (171–176, 181)**, notification bus + basics **AI (419–421, 425, 430)**, monitor
  **AH (411, 413, 415)**, basic remote **J (121–123, 130, 132–133)**, defaults + install
  **AO (493–494)**
- **Milestone:** a genuinely useful zellij worktree/pin manager. Ship, dogfood,
  get users — de-risks the whole project before any AI complexity.

#### Graphical-IDE-inspired tiers

The IDE tier overlay is defined in
`docs/superpowers/specs/2026-06-10-ide-feature-tiers-design.md` and maps onto the
existing phases rather than creating a new phase taxonomy.

- **Tier 1 (Phase 1 / Phase-1 tail):** complete the AI-free shell's IDE parity:
  full native git management **Y (319–330, 601–602, 604–605)**, a backend-agnostic
  VCS layer so git _and_ jujutsu share every diff/commit/history surface **AS
  (587–600)**, file-tree management **AF (606)**, Search Everywhere \*\*M (161–170)
  - AQ (523)**, run/task configs **AQ (520–522)**, test explorer **AQ (516–518)**,
    Problems panel **AQ (519)**, and attention routing **AI (419–430), S (256), T
    (259), AQ (524)\*\*.
- **Tier 2 (Phase 4 differentiation):** deeper language/runtime tooling once the
  Tier-1 surfaces exist: DAP debugging **AQ (525–528)**, LSP navigation/symbols
  **AQ (529–532)**, worktree timeline/history **AQ (533–534) + AN (481–488)**,
  and unified layout+task templates **D (54), G (89/94/95/99), AM (480), AQ
  (535)**.

The visual-staging (**Y 601–602, 604–605**), file-management (**AF 606**), and
jujutsu/VCS-backend (**AS 587–600**) groups are a deliberate import of the
**Kyde** feature set ("git add -p, made visual" — a fast native commit/diff
client) extended to a second VCS. Deliberately **excluded**: Kyde's in-place
text editing (native editor, in-buffer find/replace, editable diff) — superzej
stays a viewer/VCS client and hands editing off to `$EDITOR` (AG). A backend
abstraction means each remaining surface ships once and works for both git and
jujutsu.

### Phase 2 — Sandbox + inference plumbing · P1 · the AI substrate

Buildable in parallel with Phase 1's tail; **validatable standalone** (point an
existing Claude Code/Codex at the proxy; run dev containers by hand) before any
orchestrator.

- Containers: **AB (349–362)**, networking **AC (363–369)**, observability
  **AD (373–376, 381–382)**. Nix devshell (359) optional here.
- **The proxy: U (271–288)** — the keystone. Then cost/limits **V (289–300)** and the
  brokerage subset of **AJ (431, 433, 434, 437, 438, 441)** (virtual keys = 287/433).
  Token reduction **W (301–308)** rides along.
- **AI gateway / context fabric AR (541–586)** layers onto the proxy and spans phases:
  the routing, caching, token-economy, and transform/interop items (552–572, 577–586)
  ride here with U/V/W; **capability-injection (541–551) presupposes the MCP server
  (AL, Phase 4)** and house tools/skills; **guardrail items (573–576) presuppose the
  egress/opsec layer (AJ)**; and the **eval hooks (581) gate any risky transform** so
  the repo's own evals — not assumption — decide whether a transformation is net-positive.
- **Milestone:** sandboxed envs + a metered, failover-capable proxy usable with
  off-the-shelf agents. AI-free users gain sandboxes too.

### Phase 3 — Agent layer · P1 · the headline

Depends on Phase 1 (shell) + Phase 2 (proxy + containers).

- **Embedded harness first** — the `termite-agent` submodule is superzej's first-party
  coding agent, hosted as the `agent` app tab. Q/S/T track against **its** roadmap
  (`apps/termite-agent/docs/ROADMAP.md`) as the source of truth.
- Orchestration core **Q (211–224)** (defer 225–228)
- **R is now secondary:** ACP client + native adapters **(229–242)** are an *additive*
  path for running *foreign* harnesses, not the headline — superzej ships its own.
- Observability **S (243–258)** (tokens/cost 249–250 light up because the proxy exists)
- Review/merge basics **T (259–263, 267–268)**
- **Milestone:** spawn, monitor, review, and merge agents across worktrees, metered
  and sandboxed.

### Phase 4 — Differentiation · P2

The "magical" layer; mostly composition of what's built.

- **Semantic git X (309–317)** → upgrades review/merge (264, 265, 266, 270). sem alone
  (309–313, 317) enriches Phase-1 git, so pull earlier opportunistically.
- **IDE Tier 2 AQ (525–535)** — DAP/LSP client substrates, debug panels,
  symbol/reference navigation, worktree timeline/history, and layout+task
  templates compose the Phase-1 shell surfaces into deeper IDE workflows.
- **Multi-forge PR/issue/review + kanban + releases AT (631–653, 672–683)** — a forge abstraction over
  **GitHub Z (331–340)** + GitLab/Gitea/Forgejo, Stage-style structured review
  (chapters/narrative/risk/assistant, AI-additive) and project boards; **Linear
  AA (341–348)** becomes one tracker provider behind it
- **API AK (445–454)** + **MCP server AL (455–466)** + governors (436) gating recursive
  spawn (461)
- **Daily-driver tiles AM (467–480)** — nearly free once pins (E) + adapters (199) exist
- Cheap moonshots: **automations (504)**, **config sync (503)**, **offline mode (509)**
- Polish: adaptive/mobile **K (135–145)**, iroh remote **(124–126, 128–129)**, full
  profiles **H (101–110)**, audit/replay **AN (482–488)**, DX **AO (489–492, 495–496)**

### Phase 5 — Long-horizon bets · P3

**Team mode (497), pair sessions (498), federation (501), whole-workspace snapshot
(508)**, adapter marketplace (206, 210), advanced container snapshot/rollback
(390–391), GPU passthrough (393), Tor service (444).

## Critical path & strategic calls

- **The proxy (271–288) is the single chokepoint of the AI half** — cost, limits,
  brokerage, offline, per-profile budgets all hang off it. Build/validate standalone
  early; #287 virtual keys is the brokerage primitive that later unlocks team mode.
- **"Configure once, every agent inherits it" (AR) is the proxy's biggest payoff** —
  one interception point turns N-harness setup (wire an MCP server / skill / system
  rule into Claude Code _and_ Codex _and_ OpenCode) into 1, translated per harness
  (#570 tool-format translation is what makes #541 "one MCP server, every harness"
  real). Two honest tensions to engineer around: **(1) injection fights caching** —
  every injected tool/skill/system block can shift the prompt prefix and bust prompt
  caching, so injection must be cache-aware (stable prefix ordering, breakpoints after
  injected blocks) or compression savings get eaten by cache misses; **(2) transforms
  can degrade quality** — aggressive compaction/summarization/injection can make agents
  worse or confuse harnesses that manage their own context (Claude Code does), so
  defaults are conservative, every transform is opt-in by policy with a per-harness
  **transparent-passthrough vs managed** mode, and the eval hooks (#581) decide net value.
- **ACP-first (229)** collapses "support the top 10 harnesses" into one integration +
  registry — do it before hand-writing native adapters.
- **Most dependencies are existing Rust crates** (bollard, iroh, sem/weave, rtk,
  tokei) — these features are _integration, not invention_. Pull forward when convenient.
- **AI-free mode is Phase 1, not a feature** — AI layers are strictly additive, so
  511–515 come for free if the shell never hard-depends on Q–W or AL.

## Deliberate defers / cut candidates

Cut: web dashboard (510), voice (499). Parked until there's a reason: marketplace/
plugin-sharing (206, 210) until users; federation/team/pair (497, 498, 501) until
single-node is excellent; recursion governors (436) only when MCP spawn (461) ships;
Tor (444) and GPU passthrough (393) as niche opt-ins.

---

## Full feature backlog

### A. Core architecture

- [~] 1. Coordinator core — `superzej-core` owns all state (in-process, not a daemon)
- [x] 2. ~~zellij substrate~~ — **REMOVED**: the native `superzej-host` compositor owns multiplexing/rendering (termwiz + portable-pty + `CenterTree`)
- [x] 3. ~~Thin zellij WASM plugins~~ — **REMOVED**: chrome (sidebar/panel/tabbar/statusbar) is in-process in `superzej-host`
- [ ] 4. ~~Daemon↔plugin IPC~~ — **N/A after strip**: no separate plugin process; the future native plugin API contract lives in `core/plugin_api.rs` (unwired)
- [x] 5. Single-binary distribution — one `superzej`(=`szhost`); no side artifacts
- [~] 6. One core, many front doors — TUI (host) + CLI verbs share `superzej-core`; API/MCP still aspirational (AK/AL)
- [ ] 7. Headless daemon — UI attaches/detaches _(not yet; host is a foreground compositor, state resurrects from SQLite)_
- [ ] 8. Daemon supervision — crash recovery _(state resurrection only; no supervisor)_
- [x] 9. Internal event bus — normalized events _(first-class `EventBus` in `superzej-core`: subscribe/publish, urgency ranking, desktop-notification derivation)_
- [x] 10. Embedded state store — sqlite
- [x] 11. Config hot-reload — without dropping sessions
- [x] 12. Structured logging

### B. Workspace bar / tree

_Rebuilt natively in `superzej-host` (`sidebar.rs` tree model + `SidebarState`
focus mode in `run.rs` + `chrome::draw_sidebar`); view state persists in the
`ui_state` DB table. Press `Alt-s` to focus the tree, then `j/k` move, `Enter`
open/collapse, `/` filter, `s` sort, `p` pin, `Space` select, `m` menu, `X` bulk
close, `<`/`>` width, digits quick-jump._

- [x] 13. Left sidebar workspace tree
- [x] 14. Workspaces = repos (top level)
- [x] 15. Worktrees nested under workspaces
- [x] 16. Collapse/expand workspaces _(native; persisted via `ui_state`)_
- [x] 17. Persist collapse state _(`ui_state` `collapse:<slug>`)_
- [x] 18. Status glyphs — branch, dirty, ahead/behind _(gix-native ahead/behind)_
- [x] 19. Running program/agent indicator per row _(agent glyph via `worktree_agent`)_
- [x] 20. Contextual auto status dots (zellaude-style) _(host-side state machine; `activity`)_
- [x] 21. Fuzzy filter the tree _(native `/` filter)_
- [x] 22. Manual reorder / pin-to-top _(native `p`; `ui_state` `pin:<key>`)_
- [x] 23. Sort modes — recent/name/activity _(native `s` cycle; persisted)_
- [x] 24. Quick-jump to numbered item
- [x] 25. Adjustable/collapsible bar width _(native `<`/`>`; persisted)_
- [x] 26. Multi-select for bulk actions _(native `Space` mark, `X` bulk close)_
- [x] 27. Row context menu _(native `m`)_
- [ ] 28. Badge counts — PRs/unread/alerts per row

### C. Workspaces (repos)

- [x] 29. Add repo as workspace
- [x] 30. Remove workspace (non-destructive)
- [x] 31. Auto-discover repos under root dir
- [x] 32. Multiple root dirs
- [x] 33. Per-workspace default base branch
- [~] 34. Per-workspace default layout
- [ ] 35. Per-workspace default program set
- [~] 36. Per-workspace keybinds
- [x] 37. Non-git directory as workspace _(workspace `kind` repo|dir; insert-only; folder glyph in sidebar)_
- [ ] 38. Workspace-level env vars
- [ ] 39. Workspace icon/color label
- [x] 40. Recent/favorite workspaces

### D. Worktrees

_Tier-2 layout/task templates generalize worktree templates (54) with native
`CenterTree` layouts, Tier-1 tasks, pins, and sandbox/container presets._

- [x] 41. Create worktree from workspace
- [x] 42. Pick base branch on create
- [~] 43. Branch naming templates
- [x] 44. Nest under workspace in bar
- [x] 45. Per-worktree branch + status
- [x] 46. Default layout opens on select
- [x] 47. Delete worktree (dirty guard)
- [~] 48. Stale worktree GC
- [x] 49. Dirty-state warning before destructive ops
- [ ] 50. Dependency sharing — hardlink/CoW node_modules etc.
- [~] 51. Per-worktree disk usage
- [ ] 52. Fork worktree (branch from existing)
- [ ] 53. Rename worktree/branch
- [ ] 54. Worktree templates — layout+programs+container preset + setup/post-create hooks (deps install, env restore; see 657)
- [~] 55. Worktree↔PR mapping
- [~] 56. Bulk worktree cleanup

### E. Pinned programs / tiles

**Slice 1 (zellij path, shipped):** pins are `pin:<name>` session tabs summoned by
`Alt-1..9` / tabbar pin chips. Global + lazy only. See `src/commands/pin.rs`,
`layouts/pin-tab.kdl`, the tabbar chip strip, and `[[pins]]` config.

**Slice 2 (native host, shipped):** the full pin/daemon system in `superzej-host`
— a `PinSupervisor` (`crates/superzej-host/src/pins.rs`) owns daemon panes
independent of tabs/visibility, a real top-**strip** chrome region
(`layout.rs::compute_with_strip`, `chrome.rs::draw_strip`), tabbar pin chips, and
`[[pins]]` extended with `args/env/label/ratio` + `location = strip|float`. Pins
launch-or-focus via `Alt-1..9`, restart per policy on PTY exit, and resurrect from
`session_state.pin_state`.

- [x] 57. Pin to top strip _(native: live strip region + tabbar chips)_
- [~] 58. Add anywhere (into active layout) _(zellij path; native via `location=layout`)_
- [x] 59. Floating/scratch pin _(native: `location = "float"`)_
- [x] 60. Global pins (everywhere) _(scope = global)_
- [x] 61. Workspace-scoped pins _(scope = workspace, matched to session id)_
- [x] 62. Pin definition in config — `[[pins]]` name/command/cwd/args/env/location/scope
- [x] 63. Eager vs lazy start _(supervisor launches eager pins at startup)_
- [x] 64. Restart-on-exit policy _(never/always/on-failure via supervisor `on_exit`)_
- [x] 65. Singleton vs multi-instance _(supervisor dedupes by name on summon)_
- [x] 66. Persist daemons across workspace switches _(supervisor outlives tab/ws swaps)_
- [x] 67. Promote running pane to pinned _(`Ctrl-Alt-P`: focused center pane → strip)_
- [x] 68. Unpin at runtime _(`Ctrl-Alt-U`; reaps the process)_
- [x] 69. Top-strip sizing/ratio _(`[strip].ratio`, per-pin `ratio`, `Ctrl-Alt-[`/`]`)_
- [x] 70. Program labels + status glyph _(label + ●/◌/✖ in strip header + chips)_
- [x] 71. Per-program env injection _(`env` map → `PtyPane::spawn_with_env`)_
- [x] 72. Health monitoring/auto-restart _(supervisor liveness + restart on PTY death)_
- [x] 73. Program adapter — launch/notify/restart spec _(`PinSupervisor::argv`/`on_exit`)_
- [x] 74. Quick-toggle visibility _(`Alt-1..9` launch-or-focus; `Ctrl-Alt-t` strip toggle)_

### F. Keybindings

- [x] 75. Launch-or-focus toggle bind
- [~] 76. Per-program custom binds
- [x] 77. Leader/prefix layer
- [x] 78. Modal keymaps (zellij modes)
- [x] 79. Fully rebindable actions
- [x] 80. Workspace/worktree quick-switch binds
- [x] 81. Pane navigation binds
- [x] 82. Keybind cheatsheet overlay
- [x] 83. Conflict detection at load
- [ ] 84. Per-profile keybind sets
- [~] 85. Per-workspace overrides
- [ ] 86. Chorded/sequence binds
- [ ] 87. Which-key hint popup
- [ ] 88. Vim/emacs presets
- [ ] 621. IDE keymap presets (VSCode/JetBrains-style) + first-launch keymap picker, per-action overrides

### G. Panes & layouts

_Tier-2 layout/task templates compose this native layout model with the Tier-1
task registry (AQ 520–522) and worktree templates (54); new work targets
`CenterTree`, not legacy zellij KDL layouts._

- [x] 89. Per-workspace layout templates (KDL)
- [x] 90. Save arrangement as default
- [x] 91. Split/resize/move/zoom/close
- [x] 92. Floating panes
- [~] 93. Stacked/tabbed panes
- [~] 94. Named switchable layouts
- [ ] 95. Layout per worktree vs workspace
- [ ] 96. Sync panes (broadcast input)
- [x] 97. Zoom/maximize toggle
- [ ] 98. Swap pane positions
- [ ] 99. Layout import/export
- [~] 100. Auto-layout by terminal size

### H. Profiles & subprofiles

Design approved (2026-06-11): `docs/superpowers/specs/2026-06-11-profiles-subprofiles-design.md`.
Locked decisions: **(1)** a profile is a complete firewall realized as a
**separate OS process + scope root** — multiple windows run concurrently, one
per profile; **(2)** shared base config + per-profile overrides; **(3)**
firewall covers state/DB, config/theme, credentials + git identity, and
sandbox/network policy. The firewall is enforced by **rerooting the process
environment once at startup** (the codebase is already env-driven). A
**subprofile** scopes a single subsystem (`workspace` / `comms` / later `ai`)
inside a profile and switches **in-process** — e.g. unified dev but Comms split
into work/personal (see AM. 479–480, 536–539 below).

- [ ] 101. Profiles (work/personal/etc.)
- [ ] 102. Per-profile workspaces
- [ ] 103. Per-profile config/keybinds/theme
- [ ] 104. Per-profile proxy keys + budgets
- [ ] 105. Per-profile credential isolation
- [ ] 106. Per-profile notification routing
- [ ] 107. Per-profile container network policy
- [ ] 108. Profile switcher
- [ ] 109. Separate state dirs per profile
- [ ] 110. Profile-scoped audit logs
- [ ] 536. Subprofiles — per-subsystem identity/storage split within a profile (Comms work/personal)
- [ ] 537. Subprofile switcher — in-subsystem, in-process rebind (teardown + bind)
- [ ] 538. Subsystem abstraction — `workspace`/`comms`/`ai` own storage + cred scope + pane set
- [ ] 539. Multi-process model — one window per profile, `flock` singleton, terminal-spawn switcher

### I. Session persistence

- [x] 111. Detach/attach
- [x] 112. Resurrection after reboot
- [x] 113. Restore tree + layouts + pins
- [~] 114. Per-session snapshots
- [ ] 115. Named saved sessions
- [~] 116. Auto-save state
- [ ] 117. Restore agent state where possible
- [~] 118. Session list/switcher
- [ ] 119. Export/import session config
- [ ] 120. Background keep-alive

### J. Remote access

- [~] 121. SSH attach
- [~] 122. Mosh support
- [~] 123. Tailscale zero-config path
- [ ] 124. iroh embedded p2p — dial by NodeId
- [ ] 125. iroh hole-punching + relay fallback
- [ ] 126. Tunnel stdio agents over iroh/ssh
- [ ] 127. Optional auth-gated web terminal
- [~] 128. Remote daemon mode — agents on remote box
- [~] 129. Local UI → remote agents
- [ ] 130. Mobile client attach (Blink/Termius)
- [ ] 131. QR/NodeID pairing for phone
- [~] 132. Connection status indicator
- [ ] 133. Reconnect/resume on drop
- [ ] 134. Bandwidth-adaptive rendering

### K. Adaptive / mobile UI

- [ ] 135. Responsive layout by size
- [ ] 136. Phone mode (single focused pane)
- [ ] 137. Tab-switch nav on narrow screens
- [ ] 138. Leader-key-first for mobile keyboards
- [ ] 139. Condensing status line
- [ ] 140. Mouse/touch focus
- [ ] 141. Compact vs full auto-select
- [ ] 142. Larger touch hit-targets
- [ ] 143. Swipe-equivalent nav binds
- [ ] 144. Font-scaling awareness
- [ ] 145. Minimal-chrome mode

### L. Status bar & widgets

- [x] 146. Current workspace/worktree/branch
- [x] 147. Dirty-state indicator
- [ ] 148. Running agent count + states
- [ ] 149. Aggregate spend widget
- [ ] 150. Tokens-per-minute widget
- [~] 151. System load widget
- [~] 152. Per-worktree disk widget
- [~] 153. Notification badges _(sidebar + panel inbox badges; statusbar badge pending)_
- [ ] 154. Now-playing / arbitrary program widget
- [ ] 155. Next calendar event widget
- [~] 156. Remote/network status widget
- [ ] 157. Proxy upstream health widget
- [~] 158. CI/PR check status widget
- [ ] 159. Composable widget config
- [ ] 160. Click-through to detail views

### M. Command palette / launcher

_Tier-1 Search Everywhere builds on this group: command/action search stays here,
while AQ 523 tracks the cross-provider aggregation of files, tasks, problems,
tests, symbols, git objects, and worktrees._

- [x] 161. Fuzzy command palette
- [x] 162. Launch any program
- [x] 163. Jump to any workspace/worktree
- [x] 164. Run any bound action
- [x] 165. Recent/MRU commands
- [x] 166. Fuzzy file open across workspace
- [~] 167. Action search by description
- [ ] 168. Palette plugins
- [~] 169. Inline argument prompts
- [~] 170. Palette preview/themes

### N. Theming & appearance

- [x] 171. OKLCH-based theme system
- [~] 172. Light/dark/auto
- [~] 173. Custom color schemes
- [ ] 174. Per-profile themes
- [x] 175. Font family/size config
- [x] 176. Nerd Font / icon support
- [~] 177. Border/padding/style config
- [x] 178. Status bar styling
- [x] 179. Tree styling
- [x] 180. Diff color config
- [x] 181. Theme hot-reload
- [ ] 182. Theme import/export/share
- [~] 183. Per-workspace accent color
- [~] 184. Transparency/opacity (where supported)

### O. Configuration

- [x] 185. Declarative config (KDL/TOML)
- [x] 186. Project-level in-repo config
- [x] 187. Live reload
- [x] 188. Validation + error surfacing
- [x] 189. Config layering — global→profile→workspace→project
- [~] 190. Secrets references (not inline)
- [ ] 191. Migration on version bump
- [~] 192. Schema docs / autocomplete
- [x] 193. Starter/example configs
- [x] 194. Env overrides
- [ ] 195. Diff/preview before apply
- [ ] 196. Backup/versioning

### P. Plugin system

- [x] 197. zellij WASM UI plugins
- [~] 198. Stable versioned plugin API
- [~] 199. Program/tile adapter plugins
- [ ] 200. Agent harness adapter plugins
- [ ] 201. Status-bar widget plugins
- [ ] 202. Command palette plugins
- [ ] 203. Notification source plugins
- [ ] 204. Theme plugins
- [ ] 205. Hooks — pre-task/post-merge/on-event
- [ ] 206. Plugin manifest + registry
- [x] 207. Plugin sandboxing/permissions
- [~] 208. Plugin hot-reload
- [ ] 209. Plugin config surface
- [ ] 210. Plugin discovery/marketplace

### Q. Agent orchestration core

- [ ] 211. Create task (prompt/spec)
- [ ] 212. Task→worktree→agent→review→merge pipeline
- [~] 213. Agent registry + normalized states
- [ ] 214. Resource-aware concurrency cap
- [ ] 215. Task queue
- [ ] 216. Follow-up prompt into live agent
- [ ] 217. Approve/answer waiting agent
- [ ] 218. Pause/resume/kill
- [ ] 219. Fork task
- [ ] 220. Rerun task
- [ ] 221. Task templates/presets
- [ ] 222. Task tagging/grouping
- [ ] 223. Task history/audit
- [ ] 224. Batch/parallel launch
- [ ] 225. Best-of-N attempts _(deferred)_
- [ ] 226. Scheduled/cron tasks — presets (hourly/daily/weekdays/weekly) + cron + RRULE + IANA timezone; target a repo or an existing worktree; `--reuse-session` to continue in the same live terminal; create-disabled → test-trigger → enable (Orca automations) _(deferred)_
- [ ] 227. Task dependencies (run-after) _(deferred)_
- [ ] 228. Task priority _(deferred)_
- [ ] 658. Agent session history + hibernation — list/resume past agent sessions per worktree; hibernate idle sessions to reclaim resources and rehydrate on demand (feeds resource-aware cap 214; history complements S 255/257 + I 117) (Orca)

### R. Agent integration protocols

_Reframed (2026-06-22): superzej's primary agent is the **embedded first-party
harness** `termite-agent` (the `agent` app tab). This group — ACP + native adapters
for **foreign** harnesses — is now an **additive, secondary** path, not the primary
one. "Primary path" below refers to ACP being the preferred way to integrate an
external harness, not to ACP being superzej's primary agent._

- [ ] 229. ACP client (primary path)
- [ ] 230. ACP session management
- [ ] 231. ACP streaming updates
- [ ] 232. ACP permission requests → UI
- [ ] 233. ACP diff rendering
- [ ] 234. ACP plan/tool-call events
- [ ] 235. ACP Registry integration (install agents)
- [ ] 236. Native adapter: Claude Code (hooks+stream-json+OTEL)
- [ ] 237. Native adapter: Codex (exec --json)
- [ ] 238. Native adapter: OpenCode (server API/SSE)
- [ ] 239. Native adapter: aider (scripting)
- [ ] 240. Top-10 harness support
- [ ] 241. Plugin adapters for the long tail
- [ ] 242. Per-harness capability detection + fallback
- [ ] 657. Agent hook passthrough — run the repo's existing `.claude/`/`.codex/` hooks when launching a harness, plus worktree setup/post-create hooks (deps install, env restore); surface `CLAUDE.md`/`AGENTS.md` in the file tree for inline editing, untouched (Orca; extends D 54, P 205, AR 547)
- [~] _(current: `pick_agent` launches claude/aider/shell as the worktree process)_

### S. Agent observability

_Tier-1 attention routing reuses the lightweight activity-dot model for agents;
rich token/tool telemetry remains Phase 3 once proxy/adapters exist._

- [ ] 243. Contextual auto status dots
- [ ] 244. abtop-style fleet view
- [ ] 245. "Working now?" live indicator
- [ ] 246. Context-window fill per agent
- [ ] 247. Wall-clock runtime per agent
- [ ] 248. Current tool/action display
- [ ] 249. Tokens burned (live)
- [ ] 250. Cost burned (live)
- [ ] 251. Loop/runaway detector
- [ ] 252. Idle vs thinking vs rate-limited
- [ ] 253. Screen-phrase matching fallback
- [ ] 254. OTEL ingestion (CC)
- [ ] 255. Per-agent activity timeline
- [ ] 256. Needs-attention surfacing
- [ ] 257. Transcript viewer
- [ ] 258. Session replay
- [ ] 655. Per-worktree status/checkpoint field — agent-writable free-text "what just happened / status / next step" string (set via a CLI verb, `--json`), surfaced in the sidebar (feeds B 28), statusbar, panel, and the attention queue; read-before-write to preserve context (Orca worktree-comment pattern)

### T. Agent review & merge

_Tier-1 attention routing keeps the existing one-key jump and review/merge flows
as the agent-specific side of the broader attention queue._

- [~] 259. Needs-attention jump (one key)
- [x] 260. Diff review pane (highlighted)
- [~] 261. Unified/side-by-side toggle
- [ ] 262. Inline comments → follow-up prompt
- [~] 263. Approve→merge / reject→discard
- [ ] 264. Entity-level diff (sem) in review
- [ ] 265. Risk scoring (inspect) on changes
- [ ] 266. AI change explanation (sem + LLM)
- [~] 267. Cycle through agents' diffs
- [ ] 268. Squash/rebase pre-merge
- [~] 269. PR creation from review
- [ ] 270. Semantic merge via weave
- [ ] 654. Per-line agent-vs-human attribution overlay — track provenance on every line an agent touches; AI/human gutter markers in the diff/review pane; reassign to human on a subsequent human edit; local-only (never written to git), exportable from the diff toolbar (Orca-style; complements entity-blame X 312)

### U. LLM proxy

- [x] 271. Dual-protocol proxy — Anthropic + OpenAI _(SSE translation)_
- [~] 272. Hook up any provider _(configurable upstreams/backends)_
- [~] 273. Aggregate models — standard/fast/free
- [x] 274. Ordered sequential failover _(+ load-balanced & speculative routing strategies)_
- [x] 275. Limit-exhaustion detection
- [x] 276. Reset-window / Retry-After tracking
- [~] 277. Automatic failback (half-open probing) _(soft cooldown + success recovery)_
- [~] 278. Per-upstream circuit breaker _(exhaustion + cooldown state per backend/model)_
- [~] 279. Retries with backoff
- [x] 280. Key/upstream load balancing _(multi-key lanes + pool rotation)_
- [ ] 281. Model/tier aliasing
- [~] 282. Auto-downgrade under pressure
- [ ] 283. Local model upstreams (Ollama/vLLM)
- [ ] 284. Prompt-cache preservation (native Anthropic path)
- [x] 285. Streaming passthrough (no buffering)
- [~] 286. Tool-call field preservation
- [~] 287. Per-agent virtual keys _(virtual-key identity resolution + per-identity budgets)_
- [x] 288. Proxy managed as daemon/pinned program _(host auto-launch)_
- [~] 656. Interactive per-agent account/credential switcher — status-bar chip to hot-swap which subscription/account (or virtual key) a harness uses without re-auth; UX layer over the proxy's key load-balancing (280) + per-agent virtual keys (287); running sessions keep their account until restart (Orca hot-swap)

### V. Cost / limit / budget

- [x] 289. Per-request cost logging
- [x] 290. Spend attribution — agent/worktree/workspace
- [ ] 291. Spend-mode vs subscription-mode accounting
- [x] 292. Budget caps ($/tokens) per scope
- [x] 293. Enforce caps (refuse/downgrade) _(refuse-on-breach)_
- [ ] 294. RPM/TPM rate limiting
- [ ] 295. Daily/weekly/monthly ceilings
- [~] 296. Kill-switch on breach
- [ ] 297. Cache-hit-ratio tracking
- [~] 298. Spend history + export _(spend persisted to DB)_
- [ ] 299. Cost dashboards/charts
- [~] 300. Quota refresh tracking/forecast _(reset-window tracking)_

### W. Token reduction (rtk)

- [~] 301. Built-in rtk output compression _(native in-flight token-reduction engine)_
- [ ] 302. Auto-hook rtk into agent bash calls
- [ ] 303. rtk telemetry off by default
- [ ] 304. Per-command bypass
- [ ] 305. Route file reads through rtk
- [x] 306. Tokens-saved tracking
- [ ] 307. Configurable aggressiveness
- [ ] 308. Custom rtk filters per project

_→ The proxy track (U/V/W) graduates into the **AI gateway / context fabric** — see
**AR (541–586)** below, appended at the end with the other late groups but belonging
here thematically: in-flight tool-output compression (552) and prompt-cache
optimization (553) extend this group across every harness, not just rtk-hooked bash._

### X. Semantic git layer

- [ ] 309. sem-core integration (entity parsing)
- [ ] 310. tokei LOC/language counts
- [ ] 311. Entity-level diffs
- [ ] 312. Entity-level blame
- [ ] 313. Impact/blast-radius analysis
- [ ] 314. weave merge driver (code-only default)
- [ ] 315. Entity-claiming for multi-agent coordination
- [ ] 316. inspect risk scoring
- [ ] 317. Entity-derived commit messages
- [ ] 318. lazydiff-style review TUI

### Y. Git integration

_Tier-1 full git management is the cohesive milestone for this group: complete
native staging/commit, branch, stash, conflict, history, and rebase flows while
keeping `lazygit` as the fallback escape hatch._

- [x] 319. Per-worktree status/diff
- [x] 320. Stage/commit from TUI
- [x] 321. Merge/rebase from TUI _(sequencer flow UI)_
- [x] 322. Conflict resolution UI _(conflict chips + resolve/continue/abort)_
- [x] 323. Branch management
- [x] 324. Log/graph view
- [x] 325. Blame view
- [x] 326. Stash management
- [x] 327. lazygit pin (fallback)
- [~] 328. Commit signing _(GPG signing args plumbed through commit/cherry/revert)_
- [~] 329. Hooks-aware (pre-commit)
- [x] 330. Cherry-pick/revert _(+ continue/skip/abort)_
- [ ] 601. Word-level / intra-line diff highlighting (base vs working copy)
- [ ] 602. Center-gutter visual hunk stage/revert — "`git add -p`, made visual"
- [ ] 604. Rollback/discard window — checkbox tree of changes, optional delete of added files, per-row diff
- [ ] 605. Plain push/pull/fetch when ahead/behind upstream (non-PR fast path)

_Stale-**branch** lifecycle (distinct from worktree GC 48 / bulk cleanup 56): a
deliberate import of the **deadbranch** feature set ("clean up stale git branches
safely") — extends branch management (323) with safe detection, merge-aware
deletion, backup/restore, and a multi-select cleanup TUI. AI-free and additive._

- [ ] 659. Stale-branch detection — list branches over a configurable age threshold (default 30d) with metadata: age, merge status, author, last-commit date
- [ ] 660. Merge-aware deletion (default) — only delete merged branches unless explicitly overridden; guard unmerged work
- [ ] 661. Protected-branch + WIP/draft exclusion — never-delete set (main/master/develop/staging/production) + skip `wip/*`, `draft/*` patterns
- [ ] 662. Delete-with-backup — record deleted-branch SHAs to a recovery store before removal
- [ ] 663. Backup management — list / restore / cleanup deleted-branch backups + storage stats
- [ ] 664. Dry-run preview — show candidate deletions (and why each qualifies) without executing
- [ ] 665. Local + remote branch scope — operate on local and remote-tracking branches
- [ ] 666. Repo branch-health stats — aggregate counts (stale / merged / unmerged / total) for the workspace
- [ ] 667. Interactive multi-select cleanup TUI — vim nav, fuzzy filter, multi-column sort; reuses the sidebar/palette multi-select model (B 26)
- [ ] 668. Personal/author branch filter — restrict candidates to the current user's branches _(deadbranch roadmap)_
- [ ] 669. PR-aware staleness — gate deletion on merged/closed PR status _(deadbranch roadmap; via Z 331/336, generalized by AT 638)_
- [ ] 670. Stale-branch report export — JSON/CSV of detected/deleted branches _(deadbranch roadmap)_
- [ ] 671. Per-repo cleanup config — thresholds + exclusion patterns in project config _(deadbranch roadmap; rides 186 project-level config)_

### Z. GitHub

- [x] 331. PR tracking
- [x] 332. CI checks status
- [~] 333. PR review comments
- [~] 334. Issues
- [x] 335. Create PR from worktree _(+ draft/ready toggle + auto-merge enable/disable)_
- [x] 336. PR↔worktree mapping
- [x] 337. Review/approve from TUI
- [~] 338. PR event notifications
- [x] 339. gh CLI integration
- [ ] 340. Multi-repo PR dashboard (gitv-style)

### AA. Linear / issues

- [ ] 341. Linear issue list
- [ ] 342. Issue↔task↔worktree linkage
- [ ] 343. Move issue status on merge
- [ ] 344. Branch/worktree from issue
- [ ] 345. Comment on issues from TUI
- [ ] 346. Cycle/sprint view
- [ ] 347. Linear MCP/API integration
- [ ] 348. Generic tracker adapter (Jira etc.)

### AB. Container management

- [~] 349. bollard Docker/Podman control
- [x] 350. Sandbox per worktree
- [~] 351. "4 containers in directory" support
- [~] 352. Spawn/stop/restart
- [x] 353. Easy shell-in
- [~] 354. Preloaded LLM-expected tools
- [x] 355. BYO image substitution
- [~] 356. Resource caps (cgroup)
- [~] 357. Per-container env from broker
- [~] 358. devcontainer.json support
- [~] 359. Nix devshell per worktree
- [~] 360. Ephemeral reset between runs
- [x] 361. Container↔worktree binding
- [x] 362. Default-on with --no-sandbox escape

### AC. Container networking

- [~] 363. Per-container firewall
- [~] 364. Egress presets — offline/proxy-only/full
- [ ] 365. Container DNS proxy
- [ ] 366. Single auditable egress point
- [ ] 367. Shared chokepoint with LLM proxy
- [ ] 368. Open-port detection
- [ ] 369. One-click open in browser
- [ ] 370. Friendly local hostnames (worktree.localhost)
- [ ] 371. Reverse proxy for ports
- [~] 372. Block/allow lists per container

### AD. Container observability

- [~] 373. Per-container CPU/MEM
- [ ] 374. Repo-aggregate stats
- [ ] 375. Bottom stats strip per container
- [ ] 376. Shell command log
- [ ] 377. Full process-tree audit (eBPF/auditd)
- [ ] 378. Network/DNS request log
- [ ] 379. Filesystem-diff (what changed inside)
- [ ] 380. Package-install detection
- [ ] 381. Unified activity timeline — shell+proc+net+fs
- [ ] 382. Live "what's it doing" view
- [ ] 383. Container log streaming
- [ ] 384. Suspicious-behavior alerts

### AE. Container provisioning

- [ ] 385. CoW overlay from base image
- [ ] 386. Prewarmed pool (fast spawn)
- [ ] 387. Intelligent resource caching (node/cargo/pip)
- [ ] 388. Shared cache across worktrees
- [ ] 389. Auto cache cleanup
- [ ] 390. Snapshot/checkpoint (CRIU/commit)
- [ ] 391. Rollback container state
- [ ] 392. Image build cache
- [ ] 393. GPU passthrough
- [ ] 394. Base image catalog/templates

### AF. File viewer / search

- [x] 395. File tree per worktree
- [x] 396. Preview pane (tree-sitter highlight)
- [x] 397. Fuzzy file finder (skim)
- [x] 398. ripgrep project search
- [~] 399. Image preview (kitty/iTerm/sixel)
- [x] 400. Hex/binary view
- [x] 401. Open in editor
- [~] 402. Recent files
- [ ] 403. Bookmarks/marks
- [ ] 404. Diff-against-branch from file
- [ ] 606. File management from the tree — new/rename/delete (with confirm), file-type icons, git/VCS-status colors

### AG. Editor integration

- [x] 405. Open in $EDITOR (helix)
- [x] 406. Open in split/new tab
- [ ] 407. GUI editor handoff
- [~] 408. Jump to file:line from logs/diffs
- [~] 409. Editor as pinned tile _(opens as a floating tool, not a true pin)_
- [ ] 410. Per-workspace editor override

### AH. Resource / system monitoring

- [x] 411. System CPU/MEM/disk/net pane
- [ ] 412. Per-process attribution _(sysinfo system component only)_
- [x] 413. Per-worktree disk usage
- [x] 414. GPU monitor
- [x] 415. btop pin option
- [ ] 416. Historical resource charts
- [ ] 417. Threshold alerts
- [ ] 418. Network throughput per agent/container

### AI. Notifications

_Tier-1 attention routing uses this group for the event→action bus, desktop
notifications, and aggregation. AQ 524 extends the same attention model to
non-agent processes and plain task panes._

- [~] 419. fs-watch triggers (notify) _(drives panel diff refresh; also feeds the event bus)_
- [~] 420. Rules engine — event→action _(fixed event→notification mapping + urgency thresholds; no user-defined action rules yet)_
- [x] 421. Desktop notifications _(via `notify-send`, gated by `desktop_min_urgency`; not the notify-rust crate)_
- [ ] 422. Push to phone (ntfy)
- [ ] 423. Push to phone (Telegram)
- [~] 424. Per-event opt-in _(urgency-threshold gating, not yet per-event)_
- [x] 425. Contextual tree dots _(activity-dot state machine)_
- [ ] 426. Do-not-disturb / quiet hours
- [ ] 427. Per-profile routing
- [x] 428. Notification history/center _(notifications panel section + inbox, Enter-to-expand)_
- [ ] 429. Sound/bell config
- [x] 430. Aggregated bus across all sources _(core `EventBus` aggregates PR/agent/test/log/worktree/process events)_

### AJ. Security / opsec

- [ ] 431. Credential brokerage — agents never see raw keys
- [ ] 432. Scoped capability tokens per agent
- [ ] 433. Per-agent virtual keys
- [~] 434. Egress consolidation + audit
- [~] 435. Approval gates — push/rm/exec/egress
- [ ] 436. Recursion governors — depth/fan-out
- [ ] 437. Server-enforced budgets
- [~] 438. Full audit log — commands/files/net/tool calls
- [ ] 439. Encrypted secrets store (KMS/age)
- [ ] 440. Per-profile credential isolation
- [x] 441. No-telemetry / local-only default
- [ ] 442. Log redaction
- [x] 443. Sandbox-by-default for agents
- [ ] 444. Tor/hidden-service option

### AK. API surface

- [ ] 445. HTTP/gRPC API over core
- [ ] 446. Task lifecycle endpoints
- [ ] 447. Monitoring/read endpoints
- [ ] 448. Git/worktree endpoints
- [ ] 449. Sandbox endpoints
- [ ] 450. Scoped file access endpoints
- [ ] 451. SSE/WebSocket event feed
- [ ] 452. Auth scopes/tokens
- [ ] 453. Pagination/filtering
- [~] 454. Headless CLI over the API

### AL. MCP server

- [ ] 455. MCP server over core
- [ ] 456. Tools (action verbs)
- [ ] 457. Resources (task://, fleet://)
- [ ] 458. Prompts (templates)
- [ ] 459. Elicitation (approve/answer flow)
- [ ] 460. Sampling (borrow client model)
- [ ] 461. spawn_subtask (recursive)
- [ ] 462. get_sibling_state / wait_for_task
- [ ] 463. Shared blackboard resource
- [ ] 464. check_my_budget
- [ ] 465. request_human escalation
- [ ] 466. Conversational meta-control

### AM. Daily-driver / non-code tiles

- [ ] 467. Email tile (aerc; later Ox Mail)
- [ ] 468. Matrix tile (iamb/gomuks)
- [ ] 469. IRC tile (senpai/weechat)
- [ ] 470. Discord tile (discordo)
- [ ] 471. Slack tile
- [ ] 472. RSS tile (newsboat)
- [ ] 473. Calendar tile (khal/calcurse)
- [ ] 474. Todo tile (taskwarrior/vit)
- [ ] 475. Notes tile
- [ ] 476. Music tile (rmpc/ncmpcpp)
- [~] 477. Files tile (yazi/lf)
- [ ] 478. Cross-tile actions — email→task, agent→Matrix
- [ ] 479. Unified comms inbox
- [ ] 480. Workspace presets — comms/dev/personal
- [ ] 540. Comms as a subprofile-aware subsystem — per-subprofile accounts/storage/creds (first consumer of H. 536–538; design `docs/superpowers/specs/2026-06-11-profiles-subprofiles-design.md`)

### AN. Audit / logging / replay

- [~] 481. Central event log (all sources)
- [ ] 482. Per-task replay
- [ ] 483. Session recording
- [ ] 484. Exportable audit trail
- [ ] 485. Searchable history
- [ ] 486. Retention policy config
- [ ] 487. Tamper-evident logging
- [ ] 488. OTEL metrics export (out)

### AO. Onboarding / DX

- [ ] 489. First-run setup wizard
- [~] 490. Doctor/diagnostics command
- [~] 491. Built-in help/docs
- [ ] 492. Interactive tutorial
- [x] 493. Sane out-of-box defaults
- [x] 494. Single-command install
- [x] 495. NixOS module / home-manager
- [~] 496. Update/upgrade mechanism

### AP. Long-horizon bets & modes

- [ ] 497. Multi-user / team mode — shared infra; brokerage + budgets as multi-tenancy primitives
- [ ] 498. Shared / pair sessions — live co-presence + control handoff over iroh p2p
- [ ] 501. Cross-machine federation — daemons meshed via NATS/iroh; agents run where the compute is
- [ ] 503. e2e-encrypted config sync — preferences across machines, client-side encrypted
- [ ] 504. Scriptable automations / macros — event-bus triggers → action-API actions; shares the scheduling model with 226 (cron/RRULE/presets, repo-or-worktree target, session reuse)
- [ ] 508. Whole-workspace snapshot (env+state) — Nix devshell + container checkpoint + session snapshot
- [ ] 509. Offline mode (local models only) — offline aggregate of local upstreams; graceful degradation

### AQ. IDE tooling

_Tier 1 and Tier 2 are defined in
`docs/superpowers/specs/2026-06-10-ide-feature-tiers-design.md`. This group holds
new IDE-shaped capabilities that were not already covered by existing roadmap
groups; existing git, palette, notification, layout, and editor items remain in
their original groups._

- [~] 516. Test explorer tree — discover and render runnable test targets per worktree _(test discovery in `task.rs`)_
- [~] 517. Test status rollups — pass/fail/running state in panel, sidebar, and statusbar
- [~] 518. Run/debug selected test — nearest/file/package/failed-test actions, DAP handoff later
- [~] 519. Problems / diagnostics panel — compiler/linter/config/LSP diagnostics with file:line jumps _(problems panel)_
- [~] 520. Named task registry — `[[tasks]]` (explicit config) + discovered providers (just, cargo, npm, etc.) and aliases
- [~] 521. Task lifecycle controls — run/stop/restart/rerun from palette/panel/keybinds for any task
- [~] 522. Task output capture + problem matching — feed Tests and Problems without polling
- [ ] 523. Search Everywhere provider aggregation — actions, files, symbols, tasks, tests, problems, git, worktrees
- [~] 524. Non-agent process attention routing — exited/failed/waiting panes join the attention queue _(`ProcessExited` event + exit classification/policy)_
- [ ] 525. DAP client substrate — debug adapter JSON-RPC service seam in `superzej-svc`
- [ ] 526. Debug breakpoints and stepping — continue/pause/step controls and breakpoint state
- [ ] 527. Debug variables/watch/call-stack panel — inspect runtime state in the right panel
- [ ] 528. Debug launch/attach configurations — task-backed debug profiles per workspace
- [~] 529. LSP client substrate — language-server JSON-RPC service seam in `superzej-svc`
- [ ] 530. Go-to-definition and find-references — navigate via `$EDITOR`/panel handoff, not in-place editing
- [~] 531. Document/workspace symbols — feed Search Everywhere and outline/reference views _(symbols panel)_
- [x] 532. Hover/signature/code-action preview — read-only context and previewable actions
- [ ] 533. Per-worktree local timeline — git/files/tasks/tests/agents/checks activity history
- [ ] 534. Restore/compare from local timeline — inspect or recover local snapshots where available
- [ ] 535. Unified layout+task template — native `CenterTree` layout + tasks + pins + sandbox preset

### AR. AI gateway / context fabric

_The proxy track (**U/V/W**) graduating into an **AI gateway / context fabric**: the
proxy becomes the **AI control plane** — one interception point all model traffic
crosses, so any cross-cutting concern is **configured once and inherited by every
harness**, translated to each harness's format. Appended at the end with the other
late groups, but it belongs thematically right after U/V/W. Dependency tags: `[AL]`
presupposes the **MCP server** (Phase 4), `[AJ]` presupposes the **egress/opsec
guardrail layer**, `[581]` is gated by the **eval hooks**. Two engineering invariants
for the whole group: **injection must be cache-aware** (stable prefix ordering +
breakpoints after injected blocks, or it busts prompt caching), and **transforms are
conservative + opt-in by policy** with a per-harness transparent-passthrough vs managed
mode, proven net-positive by #581 rather than assumed (Claude Code manages its own
context — don't fight it)._

**Capability injection — register once, all agents inherit it:**

- [ ] 541. Central MCP registry — register an MCP server once; proxy advertises its tools to every agent, translated per-harness `[AL]`
- [ ] 542. MCP lifecycle management — proxy spawns/supervises/health-checks/connection-pools MCP servers; one instance shared across agents `[AL]`
- [ ] 543. MCP credential brokerage — proxy holds the MCP server's secrets; agents get the tools, never the keys `[AL]` _(extends AJ 431)_
- [ ] 544. Skill injection — register SKILL.md-style skills once; inject the relevant ones by task/context
- [ ] 545. House-tool injection — auto-add built-ins (rtk, sem, weave, guardrails) to every agent's toolset
- [ ] 546. Tool filtering/override — hide dangerous tools, override descriptions, enforce a per-policy toolset
- [ ] 547. System-prompt layering — inject house rules, coding standards, repo context (AGENTS.md/CLAUDE.md) uniformly across harnesses
- [ ] 548. Prompt/template library — shared, versioned prompt snippets injected on demand
- [ ] 549. Context/resource auto-attach — pull in repo docs, schemas, style guides relevant to the task
- [ ] 550. Cross-session memory injection — persistent per-project/agent notes injected as context
- [ ] 551. Role/persona presets — inject sub-agent personas centrally

**Context & token economy — rides W, applied to every harness:**

- [~] 552. In-flight `tool_result` compression — rtk-style, applied to result blocks regardless of how the command ran _(native token-reduction engine; extends W 301/305)_
- [ ] 553. Prompt-cache optimization — structure requests for max cache hits, insert breakpoints, track savings _(extends U 284, V 297; the biggest cost lever)_
- [ ] 554. Context-window management — auto-summarize/compact near the limit; sliding window; context GC `[581]`
- [ ] 555. Semantic dedup — strip files/outputs already present in context; never re-send the same file twice
- [ ] 556. Diff-aware context — send only changed regions of files instead of whole files
- [ ] 557. History trimming — strip verbose thinking / old turns before resending `[581]`
- [ ] 558. Attachment optimization — downscale/compress images and PDFs before they cost tokens
- [ ] 559. Budget shaping — allocate the context window across system/tools/history/docs deliberately

**Intelligent routing — extends U:**

- [ ] 560. Semantic/task-type routing — classify the prompt → cheap model for simple turns, strong for hard
- [ ] 561. Cost-aware tiering — cheapest model that clears a quality bar
- [ ] 562. Cascade/speculative — try cheap first, escalate on low confidence or failure
- [ ] 563. Best-of-N / ensembling at the proxy — fan one request to several models, pick or merge _(see Q 225; feeds eval 581)_
- [ ] 564. Tool-strength routing — tool-heavy requests → tool-reliable models
- [ ] 565. Local-first routing — prefer a local model when adequate _(powers 509 offline mode)_

**Cross-agent caching & memory:**

- [ ] 566. Exact + semantic response cache — two agents asking the same thing hit cache
- [ ] 567. Tool-result cache — don't re-run the same `grep`/`ls` for two agents
- [ ] 568. Embedding/rerank cache + a proxy-fronted embeddings + rerank endpoint
- [ ] 569. Shared fleet knowledge cache — the blackboard, at the inference layer _(see AL 463)_

**Transformation & interop:**

- [ ] 570. Tool-format translation — what makes "one MCP server, every harness" actually work `[AL]` _(underpins 541)_
- [ ] 571. Capability shimming — emulate JSON-mode / structured output / tool-use for models that lack them
- [ ] 572. Output validation/repair — schema conformance, JSON repair, reject hallucinated tool calls _(protocol translation itself = U 271)_

**Safety, guardrails & egress (opsec):**

- [ ] 573. Prompt-injection scanning on tool results — scan fetched pages/files before they re-enter context; quarantine `[AJ]` _(pairs with AD 384)_
- [ ] 574. Secret detection in prompts — block an API key/credential from being sent to a model `[AJ]` _(extends AJ 442)_
- [ ] 575. PII/redaction on egress — scrub before prompts leave the box `[AJ]` _(extends AJ 442)_
- [ ] 576. Per-policy content filtering — optional, off by default `[AJ]`
       _(loop/runaway detection + kill-switch already S 251 / V 296 — now enforced at the one chokepoint all traffic crosses)_

**Observability, cost & eval:**

- [ ] 577. Per-request/agent/tool token + cost accounting _(extends V 289/290)_
- [~] 578. Compression-savings + cache-hit-ratio tracking _(tokens-saved metric; extends V 297, W 306)_
- [ ] 579. Tool-call analytics — which tools agents actually use, latency, failure rates
- [ ] 580. Context-utilization tracking — how full the window runs per agent _(extends S 246)_
- [ ] 581. Quality/eval hooks — score responses; A/B transformations to prove they help (the eval harness, ex-505/506; gates 554/557/risky transforms)
- [ ] 582. Full request/response audit + replay — exact context inspection, time-travel _(extends AN 482)_

**Dev/ops affordances:**

- [ ] 583. Replay with a different model — debugging and migration
- [ ] 584. Request inspector — see the exact context that was sent
- [ ] 585. Record/mock mode — run agents against recorded responses offline, for testing
- [ ] 586. Cost dry-run — "what would this conversation cost on model X"

### AS. Version-control backends (git + jujutsu)

_The bridge that makes "every feature, for both git **and** jujutsu" real: a
pluggable VCS provider behind one trait so every diff/status/commit/history/branch
surface (Y, X, T, and the visual-staging items Y 601–602, 604–605) routes through it once
and works on either backend. jj's change-centric model (working-copy-as-a-commit,
first-class conflicts, operation log) maps onto the same panel/sidebar/gutter UI
rather than forking it. **Viewer + VCS-operations only — no in-place text
editing**; editing stays handoff via AG. AI-free and additive — pull forward
opportunistically alongside basic git (Phase 1)._

- [ ] 587. VCS backend abstraction — `git` | `jj` provider trait; all diff/commit/history/branch surfaces route through it
- [ ] 588. Jujutsu backend — jj-native status/diff/log via `jj` CLI (+ jj-lib reads where available), CLI fallback like the GitRouter
- [ ] 589. Colocated git+jj repos — operate over `.jj` and `.git` together; detect backend per workspace/worktree
- [ ] 590. Change-centric model — working-copy-as-a-commit; surface change IDs vs commit IDs in panel/sidebar
- [ ] 591. `jj describe` — edit change descriptions (the commit-message-box equivalent)
- [ ] 592. `jj new` / `edit` / `abandon` — create, switch-to-edit, and drop changes
- [ ] 593. `jj squash` / `split` — move hunks between changes; the center-gutter staging (602) maps to squash/split
- [ ] 594. `jj rebase` / `restore` — re-parent changes, restore paths (maps onto Y 321/330)
- [ ] 595. Bookmarks — jj bookmark create/move/delete, mapped onto the branch switcher (Y 323)
- [ ] 596. Operation log + undo/redo — `jj op log`, `jj undo`, `jj op restore` (the rollback window, jj-flavoured)
- [ ] 597. First-class conflict handling — show/resolve jj's in-tree conflicts in the diff/merge UI (Y 322)
- [ ] 598. Revset-powered log/graph view — `jj log` revsets feed the graph (Y 324)
- [ ] 599. jj fetch/push to git remotes — incl. PR/worktree mapping (Z 336, Y 605)
- [ ] 600. jj workspaces ↔ superzej worktree-tab model — map `jj workspace` onto the per-worktree tab/sidebar
- [ ] 622. Repo adoption — `jj git init` (colocate in an existing git worktree) + `jj git clone` (fresh jj repo); auto-detect and offer to adopt (extends 589)
- [ ] 623. `jj absorb` — auto-distribute working-copy edits into the ancestor changes that last touched each line (the "smart squash", no git equivalent)
- [ ] 624. `jj duplicate` / `jj backout` — copy a change elsewhere; create an inverse change (jj's revert)
- [ ] 625. `jj evolog` — per-change evolution history, distinct from the operation log (596)
- [ ] 626. File tracking — `jj file track`/`untrack` + filesets; surface jj's auto-snapshot model vs git's index
- [ ] 627. Remote-bookmark tracking — `jj bookmark track`/`untrack`, push `--allow-new`; tracked/ahead/behind per remote (extends 595/599)
- [ ] 628. jj commit signing — GPG/SSH signing for jj changes (parallels git 328)
- [ ] 629. `jj resolve` — external merge-tool flow + conflict materialization/round-trip (extends in-UI conflicts 597)
- [ ] 630. Advanced history rewriting — `jj parallelize`, `jj simplify-parents`, and other revset-targeted rewrites (extends 594)

### AT. Multi-forge PR/MR, issues, reviews, boards & releases (GitHub/GitLab/Gitea/Forgejo)

_Does for code-forges what AS does for VCS backends: one provider trait so PR/MR,
issue, review, comment, board, and CI surfaces work across **GitHub, GitLab,
Gitea, and Forgejo** (and self-hosted instances), generalizing the GitHub-only Z
group. It also imports the **Stage** (stagereview.app / `stagereview` CLI) review
workflow — break a diff into ordered "chapters", surface intent/risk, and a
review-plan assistant that cites exact `file:line`. Split by AI-dependence: the
forge plumbing, dashboard, comments/reviews, boards, local-diff review, and
notifications are **AI-free**; the narrative/risk/assistant layer is
**AI-additive via the proxy** (can target local models for the local-first
posture) and degrades to a plain diff when AI is off._

- [ ] 631. Forge backend abstraction — pluggable provider trait; PR/MR, issue, review, comment, board, CI surfaces route through it (generalizes Z the way AS generalizes Y)
- [ ] 632. GitHub provider — `gh`/octocrab; the existing Z (331–340) becomes the reference implementation
- [ ] 633. GitLab provider — merge requests, issues, notes, pipelines via GitLab API / `glab`
- [ ] 634. Gitea provider — PRs, issues, reviews via Gitea API / `tea` CLI
- [ ] 635. Forgejo provider — Forgejo API (Gitea-compatible + Forgejo extensions)
- [ ] 636. Self-hosted / enterprise endpoints — per-instance base URL + token/SSO config per forge
- [ ] 637. Unified cross-forge PR/MR dashboard — every PR/MR across repos & forges, grouped Ready-to-review / Yours / Recently-completed (extends Z 340)
- [ ] 638. PR/MR triage states — needs-review / changes-requested / approved / mergeable, reviewer + comment counts, age, ± (feeds sidebar badge counts B 28)
- [ ] 639. Structured "chapters" — break a diff into ordered, themed groups (intent + dependencies + the files that matter) with per-chapter review progress
- [ ] 640. Local working-tree review — chapters over staged/unstaged/untracked or any `base..compare` diff, before a PR exists (extends Y 319; the stage-cli `--base/--compare/--ref/--pr` model)
- [ ] 641. `.stageignore` exclusions + "Other changes" catch-all — gitignore-style patterns scope what review analyzes; excluded files still surfaced, never silently hidden
- [ ] 642. PR narrative / "prologue" — why-this-PR / what-it-does / key-changes summary (AI via proxy; plain diff when AI-free)
- [ ] 643. Review-focus / risk callouts — surface the riskiest files/hunks with reasoning (ties to X 316 inspect, T 265)
- [ ] 644. Review-plan assistant ("Stagent") — what-to-review-first / what's-risky / how-this-fits, answers citing exact `file:line` (ties to T 266; via proxy)
- [ ] 645. Threaded review comments — read/post/resolve inline + top-level per forge, plus a local review-comments model (stage-cli) for pre-PR diffs
- [ ] 646. Two-way comment & approval sync — comments/approvals/review state round-trip with the forge; status checks, required reviews, and merge rules preserved
- [ ] 647. Submit a review — approve / request-changes / comment with batched line comments + apply-suggestion round-trip
- [ ] 648. Cross-forge issue list/triage — extends AA's generic tracker (348) to GitHub/GitLab/Gitea/Forgejo issues
- [ ] 649. Issue ↔ worktree/branch/PR linkage — branch/worktree from an issue, auto-close on merge (generalizes AA 342–344)
- [ ] 650. Kanban / project boards — Gitea/Forgejo/GitLab boards + GitHub Projects: view columns, move cards, WIP at a glance
- [ ] 651. Board card ↔ worktree/PR binding — open a card's branch as a worktree tab; reflect PR/CI state back on the card
- [ ] 652. Cross-forge notification feed — review-requested / mentioned / CI-failed / merged events into the notification bus (AI 419–430)
- [ ] 653. CI/checks status across forges — checks, required gates, mergeability per PR/MR (generalizes Z 332)

_Release & tag management — the forge surface the GitHub-only Z group and the
multi-forge AT group both skip. A deliberate import of the **brows**
(`rubysolo/brows`) browsing UX ("browse GitHub releases in a TUI"), generalized
across **GitHub/GitLab/Gitea/Forgejo** behind the same forge backend trait (631)
and extended from view-only into **full management** (create/edit/delete, tags,
assets). Mostly **AI-free** — only the release-notes narrative (679) is
AI-additive via the proxy and degrades to a plain merged-PR/commit list when AI
is off. Releases flow into the notification bus (AI 419–430) and the worktree/PR
model, so this rides the existing forge, diff, and notification surfaces rather
than inventing new ones._

- [ ] 672. Release/tag ops on the forge trait — list/view/create/edit/delete releases + tags across GitHub/GitLab/Gitea/Forgejo (extends 631; GitHub `gh release`/octocrab as the reference impl, generalizing Z)
- [ ] 673. Release browser — interactive cross-forge release list (tag, title, date, author, draft/prerelease/latest badges), fuzzy filter + version jump (the brows browsing surface, generalized)
- [ ] 674. Release detail view — rendered markdown notes, target tag/commit, asset list, and "changes since the previous release" (brows-style read view)
- [ ] 675. Tag management — list / create (lightweight + annotated) / delete tags, tag→commit jump, signed tags (parallels git commit signing 328)
- [ ] 676. Create release — from an existing or new tag; title, notes, target commit/branch; draft / prerelease / mark-as-latest flags
- [ ] 677. Edit / delete release — update title/notes/flags, delete release (dirty/confirm guard, like 47/49)
- [ ] 678. Release assets — list with sizes, download/open, upload/attach build artifacts, remove assets
- [ ] 679. Auto-generated release notes — forge-native generated notes (e.g. GitHub `generate_release_notes`) plus an AI-authored changelog/narrative via the proxy (AI-additive; falls back to a plain merged-PR/commit list when AI-free; ties to T 266, AT 642)
- [ ] 680. Version diff / changelog — compare two releases or tags: commit range + PRs/MRs merged between them (reuses the diff Y 319 and PR/MR AT 637/638 surfaces)
- [ ] 681. Release ↔ worktree/PR linkage — surface a repo's latest/relevant release in the sidebar/panel; cut a release from the current worktree's HEAD or a merged PR (generalizes Z 336, feeds B 28)
- [ ] 682. Release notifications — published / new-release / pre-release events into the notification bus and the cross-forge feed (AI 419–430, AT 652)
- [ ] 683. Per-forge release config — default target branch, tag/version naming templates, draft-by-default, asset glob patterns in project config (rides 186)

### AI-free mode (audience-widener)

- [~] 511. AI-free mode — run as a pure terminal workspace/worktree manager, no agents/proxy/LLM
- [~] 512. All features usable manually — git, worktrees, containers, pins, comms tiles, monitoring with zero AI
- [ ] 513. Compile-out AI components — feature flag for a lean binary without proxy/agent/MCP layers
- [~] 514. Graceful degradation — AI panels, dots, cost widgets simply absent; nothing else breaks
- [x] 515. No-AI privacy posture — zero outbound model traffic, smaller attack surface, fully local
