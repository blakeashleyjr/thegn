# superzej — roadmap & progress

**725 features across 50 groups (A–AX).** The list is **two tracks joined by one
keystone**: an AI-free _shell_ track and an AI track, bridged by the **proxy**. The
control plane has **two layers** (see
`docs/superpowers/specs/2026-06-24-acp-two-layer-control-plane-design.md`): the
**lower** plane is the **proxy** (**U/V/W → AR**), the single interception point
every agent's _model traffic_ crosses — configure a cross-cutting concern **once,
every harness inherits it**; the **upper** plane is **ACP** (group **R**), which owns
the _agent conversation_ (superzej as Client/Agent/Proxy). The planes meet at
`providers/set` (R 689 → U) and MCP-over-ACP (R 696 → AL). U/V/W graduates into the
**AI gateway / context fabric** in **AR (541–586)**.

Original numbering is preserved (gaps are deliberate cuts: 499/500/502/505/506/507/510;
deadbranch import 659–671 under Y; brows releases 672–683 under AT; the dropped eval
harness 505/506 resurfaces scoped as AR 581).

**Status legend:** `[x]` done · `[~]` partial · `[ ]` not started. Statuses are
verified against the codebase. See `CLAUDE.md` for architecture.

---

## Progress summary (as of 2026-07-06)

**Where we are:** **Phase 1** (the AI-free shell) is **essentially complete** — native
git management, the notification/event bus, and IDE panels (problems/tasks/tests/symbols)
have all landed. **Phase 2's substrate** (sandbox + remote) and **the proxy** (U/V/W) are
in. Since the prior audit, three big tranches shipped: **environment bundles (AU)** +
**profiles/subprofiles (H)** — the identity/credential firewall with a single pane-spawn
`compose` seam, opt-in `.env`, secret resolvers, and bundle/profile/subprofile switchers;
**notification routing (AI)** — user rules, DND/quiet-hours, per-profile routing, sound/bell;
and the **remote/terminal-hosts** stack — first-class terminal groups (ssh/mosh), native
provider exec, warm-spare sandbox pool, and sprite reliability hardening. **Time-travel
replay (AN 482)** landed. New tooling capabilities arrived: **managed-tool resolver**,
**BugStalker debugger**, **cross-worktree aggregation** (read-only "Across" panel), and
**MCP servers + capability grants**. The **CI/CD inspection** layer (AV) has Phase A complete
(+ actionable badge/panel); the **Log Analyzer** (AW) has its parser/buffer/DSL wired (UI
pending); the headless **MCP server** (AL) ships a tool/resource subset; and the **ACP client
seam (R)** is partially wired (`initialize`, `session/update`, fs/terminal surfaces,
`providers/set` routing, MCP-over-ACP). The true AI layers (Q–T, the rest of AL/R/AR) and
multi-forge/jujutsu/Windows remain unstarted. **Tally: 255 done · 91 partial · 379 not
started** of 725. _(2026-07-06 reconcile: archived `add-{placement-engine,independent-hosts,spillover-and-compute-ledger,inapp-pr-view}` — the multi-host placement engine now lives in `openspec/specs/placement`; flipped shipped-but-unchecked X 309–312/317, Z 333, B 28, L 153, AG 408/410. `add-agent-merge-driver` left in-flight (CLI slice shipped; in-TUI actions deferred). Provider live-verification + DO/Fly token rotation still outstanding.)_

**Shipped & solid:**

- **Shell core** — native `superzej-host` compositor (termwiz + portable-pty +
  `CenterTree`, `alacritty_terminal` emulator); in-process chrome, workspaces × worktree
  tabs, session detach/attach/resurrection. (Zellij/WASM stripped in Phase 0.)
- **Keybinds** — full registry, KDL splice, conflict detection, cheatsheet (`F`), chords/
  sequences + which-key, vim/emacs modes, IDE presets, per-profile/workspace/program sets.
- **Panes & layouts** — split/resize/zoom/float, stacked/tabbed (`CenterTree::Stack`),
  named save/apply, import/export, sync-panes broadcast, responsive auto-collapse.
- **Sessions** — detach/attach, reboot resurrection, snapshots, debounced auto-save.
- **Config** — declarative TOML, layering, env/flag overlays, live reload, 95%-gated core.
- **Palette** — native iocraft Cmd-K, nucleo + embedded ripgrep, Search Everywhere.
- **Git** — full native management (stage/commit, branch, log/graph, blame, stash, merge/
  rebase, conflict UI, cherry-pick/revert, signing, visual hunk staging, rollback, push/
  pull/fetch) + per-worktree diff; GitHub PR panel via `gh`; lazygit fallback.
- **Files/editor/monitor** — yazi drawer (file mgmt + git colors), fuzzy finder + ripgrep,
  `$EDITOR`, system/GPU monitors, statusbar stats, activity-dot state machine.
- **Sandbox + remote** — per-worktree podman/docker/bwrap/none, bind-mount-at-real-path,
  remote worktrees over ssh/mosh.
- **Notification/event bus** — first-class `EventBus` (PR/agent/test/log/worktree/process),
  urgency thresholds, desktop notifications, notifications panel/inbox, sidebar badges.
- **Proxy** — `superzej-proxy`: dual-protocol relay (Anthropic SSE + OpenAI), failover/
  load-balanced/speculative routing, limit/reset tracking, per-scope budgets + spend
  attribution, in-flight token reduction; host auto-launches it.
- **IDE panels** — problems/diagnostics, task registry + test discovery, symbols, LSP
  preview substrate (hover/signature/code-action).
- **CI/CD (AV Phase A)** — `CiProvider` trait + normalized run→job→step model, GitHub
  Actions + GitLab CI providers, panel `Section::Ci`, off-loop poller, CLI + statusbar
  badge, first-failure scan.
- **Pins (E, 57–74)** — config-driven `PinSupervisor` owning daemon panes across tab/
  workspace switches, top-strip + tabbar chips, eager/lazy, restart/health, promote/unpin,
  resurrect via `session_state.pin_state`.
- **Env bundles + profiles (AU 735–748, H 101–109/536–539)** — one `compose` seam routes
  every pane spawn (shell + agent) through named bundles; clear-then-allowlist base env,
  opt-in allowlisted `.env`, secret resolvers, per-scope binding; profiles are firewalled
  separate processes (reroot + `flock` singleton) with in-process subprofile rescoping;
  bundle/profile/subprofile switchers. See `openspec/specs/{env-bundles,profiles}`.
- **Notification routing (AI 420/424/426/427/429)** — user-defined event→action rules,
  DND/quiet-hours, per-profile routing, sound/bell tiers. See `openspec/specs/notifications`.
- **Remote / terminal-hosts (J, terminal-hosts, provider-native-exec)** — first-class
  terminal groups (ssh/mosh) in the sidebar, native provider exec (CLI-free managed
  providers), warm-spare sandbox pool, sprite reliability hardening. See
  `openspec/specs/{terminal-hosts,provider-native-exec}`.
- **Time-travel replay (AN 482)** — per-pane recording ring + `Alt+r` scrub/search +
  vim registers. See `openspec/specs/time-travel`.
- **New tooling capabilities** — managed-tool resolver, BugStalker debugger, cross-worktree
  aggregation (read-only "Across" panel), MCP servers + capability grants. See
  `openspec/specs/{managed-tools,debugger,cross-worktree-aggregation,mcp-servers,capability-grants}`.

**Notable remaining gaps (candidate next work):**

- **Agent layer (Q–T) + ACP (R) + rest of MCP (AL)** — the headline AI track. The
  earlier embedded-`termite-agent`-tile path (an `agent` app tab driving `AgentRuntime`
  on `spawn_blocking`) was **reverted** — the tab was dropped, `apps/agent.rs` is an inert
  stub, and there is no `AgentRuntime`. The **shipping agent surface is a managed `pi`
  driven over ACP** (R1 client committed on `main`: `crates/superzej-{svc,core}/src/acp/`,
  `extensions/superzej-acp.ts`, host `bouncer.rs`/`relay.rs`), routed through `szproxy`
  under per-worktree virtual keys. R is the upper control plane (ACP Client/Agent/Proxy).
  Orchestration core **Q** (task registry → worktree → agent → review → merge pipeline +
  queue) is **unstarted**; **S** observability and **T** review/merge are largely
  unstarted (diff pane 260 aside). See `openspec/specs/agent/spec.md` (behavior of
  record) and the two-layer-control-plane spec; the embedded-agent spec is SUPERSEDED
  history.
- **Notification polish tail** — push-to-phone (422 ntfy / 423 Telegram) and in-app
  diagnostic surfacing / toasts (749–753). The routing engine, DND, per-profile routing,
  and sound/bell (420/426/427/429) all landed.
- **IDE Tier 1 tail** — GUI editor handoff / per-workspace editor override (407/410);
  badge PR-count data source (28). Search Everywhere, visual staging, problems/tasks/tests/
  symbols all landed.
- **Statusbar AI widgets (148–150, 157)** — gated on the agent/proxy-UI layer.
- **Imports not yet started** — Orca adds (654–658: AI/human line attribution, worktree
  status field, account hot-swap chip, hook passthrough, session hibernation), deadbranch
  stale-branch cleanup (659–671), brows release mgmt (672–683), jujutsu VCS backend (AS),
  multi-forge (AT), native Windows (AX), Log Analyzer UI (AW 721/723–728). (Env bundles AU
  now landed — Phase A–C done, dotfile tiers D partial.)
- **Media player** (AM 476, optional `[media]` feature, off by default) and the headless
  **MCP server** (AL 455–457/461/464/465) landed since the prior audit.

---

## OpenSpec is the source of truth

**This file is the _map_; `openspec/specs/<capability>/spec.md` is the _territory_.**
Where a capability spec exists, it — not this roadmap — is the authoritative,
behavior-first record of how the shipped `szhost` behaves _today_ (`### Requirement:`
SHALL/MUST + `#### Scenario:` WHEN/THEN). This roadmap owns the numbered backlog and
the phasing; OpenSpec owns per-capability detail. When they disagree, the spec wins
and this file is stale — fix it. In-flight work lives in `openspec/changes/<name>/`
and merges into `openspec/specs/` on archive (`/opsx:archive`), at which point the
corresponding roadmap items flip to `[x]`.

**Capability index (32 specs) → roadmap groups.** Run `just openspec validate --all
--strict` to confirm all specs + changes are green (currently 59/59).

| OpenSpec capability                                 | Roadmap group(s)                                 |
| --------------------------------------------------- | ------------------------------------------------ |
| `event-loop`, `rendering`, `state-db`, `perf-suite` | A. Core architecture / perf invariants           |
| `sidebar`, `navigation`                             | B. Workspace bar/tree · L. Status bar            |
| `workspace`                                         | C. Workspaces                                    |
| `pins`                                              | E. Pinned programs                               |
| `keybindings`                                       | F. Keybindings                                   |
| `profiles`                                          | H. Profiles & subprofiles                        |
| `env-bundles`                                       | AU. Environment bundles                          |
| `command-palette`                                   | M. Command palette                               |
| `theming`, `terminal-compat`                        | N. Theming · terminal degradation                |
| `agent`                                             | Q/R/S/T. Agent layer                             |
| `managed-tools`, `debugger`                         | managed-tool resolver · BugStalker (AQ-adjacent) |
| `cross-worktree-aggregation`                        | S. Agent observability ("Across" panel)          |
| `mcp-servers`, `capability-grants`                  | AL. MCP server · AJ 432 scoped tokens            |
| `llm-proxy`                                         | U/V/W. Proxy / cost / token reduction            |
| `git-backend`                                       | Y/X. Git integration / semantic git              |
| `file-explorer`                                     | AF. File viewer/search                           |
| `sandbox`, `provider-native-exec`, `terminal-hosts` | AB/AC. Containers · J. Remote                    |
| `placement`                                         | AE. Provisioning · J. Remote (multi-host broker) |
| `notifications`                                     | AI. Notifications                                |
| `panel`, `test-explorer`                            | Panel tabs · AQ. IDE tooling                     |
| `ci-inspection`                                     | AV. CI/CD inspection                             |
| `time-travel`                                       | AN 482. Per-task replay                          |

Groups without a capability spec yet (AA Linear, AK API, AM tiles, AR gateway, AS
jujutsu, AT multi-forge, AW Log Analyzer, AX Windows, …) are governed by the
`docs/superpowers/{plans,specs}/` design docs until their first `/opsx:propose`.

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
- **R is now secondary:** ACP client + native adapters **(229–242)** are an _additive_
  path for running _foreign_ harnesses, not the headline — superzej ships its own.
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
- [~] 6. One core, many front doors — TUI (host) + CLI verbs share `superzej-core`; API/MCP still aspirational (AK/AL) _(CLI surface v2 — `wt`/`repo` namespaces, headless `wt new`/`rm`, blanket `--json`, grouped help, completions, `open` remote control: `openspec/changes/add-cli-namespaces-and-remote-open/`)_
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
- [x] 28. Badge counts — PRs/unread/alerts per row _(`sidebar.rs` per-row `unread_count`/`alert_count`/`pr_count` maps, `pr_count` populated from `db.get_open_pr_counts_by_branch` in `hydrate.rs`, rendered in chrome)_

### C. Workspaces (repos)

- [x] 29. Add repo as workspace
- [x] 30. Remove workspace (non-destructive)
- [x] 31. Auto-discover repos under root dir
- [x] 32. Multiple root dirs
- [x] 33. Per-workspace default base branch
- [~] 34. Per-workspace default layout
- [ ] 35. Per-workspace default program set
- [x] 36. Per-workspace keybinds _(`WorkspaceConfig::keybinds`; `[workspace.<name>.keybinds]`)_
- [x] 37. Non-git directory as workspace _(workspace `kind` repo|dir; insert-only; folder glyph in sidebar)_
- [x] 38. Workspace-level env vars _(subsumed by env bundles — AU 736; a workspace binds a bundle via `[workspace.<slug>].env_bundle`)_
- [ ] 39. Workspace icon/color label
- [x] 40. Recent/favorite workspaces _(upgraded to a frecency-ranked `~` palette opener + connect-to-root + clone-and-open + tmuxinator/sesh layout import — openspec `2026-07-03-add-frecency-navigation`)_

### D. Worktrees

_Tier-2 layout/task templates generalize worktree templates (54) with native
`CenterTree` layouts, Tier-1 tasks, pins, and sandbox/container presets._

- [x] 41. Create worktree from workspace
- [x] 42. Pick base branch on create
- [x] 43. Branch naming templates _(`config.rs` `branch_prefix` + `NameScheme` enum Words/Numbered)_
- [x] 44. Nest under workspace in bar
- [x] 45. Per-worktree branch + status
- [x] 46. Default layout opens on select
- [x] 47. Delete worktree (dirty guard)
- [x] 48. Stale worktree GC _(auto `cargo clean` of `target/` on PR merge/close via the `pr_branch_cache` feed → `worktree::clean_target`, gated `[disk].auto_clean_on_merge`/`clean_on_pr_closed`; active worktree + running builds skipped)_
- [x] 49. Dirty-state warning before destructive ops
- [ ] 50. Dependency sharing — hardlink/CoW node_modules etc.
- [x] 51. Per-worktree disk usage _(off-loop `du` scan → `worktree_disk` cache (db v20), sidebar size badge + statusbar `[disk].warn_threshold_gb` chip + `superzej disk`/`clean` CLI; `disk.rs`)_
- [x] 52. Fork worktree (branch from existing) _(`SidebarOutcome::Fork` from the row menu → `begin_worktree_wizard(base_override=Some(branch))` in run.rs)_
- [x] 53. Rename worktree/branch _(`PromptRename` → `HostInputKind::RenameWorktree` → `superzej_core::worktree::rename` (branch -m + worktree move); tested by `rename_moves_branch_and_checkout`)_
- [~] 54. Worktree templates — layout+programs+container preset + setup/post-create hooks (deps install, env restore; see 657) _(`NewWorktreeFromTemplate` action wired in run.rs + `[[worktree_templates]]` config; setup/post-create hook depth still partial)_
- [x] 55. Worktree↔PR mapping _(`pr_branch_cache` keyed by repo root; `get_open_pr_counts_by_branch`/`spawn_pr_cache_refresh` in hydrate.rs — every worktree resolves its branch's badge)_
- [x] 56. Bulk worktree cleanup _(sidebar multi-select `Space` + `X` bulk close)_

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
- [x] 76. Per-program custom binds _(`config.rs` `program_keybinds` + `program_remap`)_
- [x] 77. Leader/prefix layer
- [x] 78. Modal keymaps (zellij modes)
- [x] 79. Fully rebindable actions
- [x] 80. Workspace/worktree quick-switch binds
- [x] 81. Pane navigation binds
- [x] 82. Keybind cheatsheet overlay
- [x] 83. Conflict detection at load
- [x] 84. Per-profile keybind sets _(`config.rs` `profiles: BTreeMap<_,ProfileConfig>`w/ per-profile`keybinds`+`default*mode`)*
- [x] 85. Per-workspace overrides _(`WorkspaceConfig::keybinds`)_
- [x] 86. Chorded/sequence binds _(`sequence.rs` `SequenceMatcher` `feed`/`add_sequence`, wired in run.rs)_
- [x] 87. Which-key hint popup _(`SequenceMatcher::pending_continuations()` → statusbar keyhints)_
- [x] 88. Vim/emacs presets _(`keymap.rs` `Mode::{VimNormal,VimInsert,Emacs}` + `SwitchMode`)_
- [x] 621. IDE keymap presets (VSCode/JetBrains) + first-launch keymap picker, per-action overrides _(`keymap_preset` config → `apply_keymap_preset` overlays IDE chords onto existing actions, applied before user `[keybinds]` so per-action overrides win; one-time `menu::keymap_preset_menu` picker on first launch, choice persisted in `ui_state`)_

### G. Panes & layouts

_Tier-2 layout/task templates compose this native layout model with the Tier-1
task registry (AQ 520–522) and worktree templates (54); new work targets
`CenterTree`, not legacy zellij KDL layouts._

- [x] 89. Per-workspace layout templates (KDL)
- [x] 90. Save arrangement as default
- [x] 91. Split/resize/move/zoom/close
- [x] 92. Floating panes
- [x] 93. Stacked/tabbed panes _(`center.rs` `CenterTree::Stack`)_
- [x] 94. Named switchable layouts _(`SaveLayout`/`ApplyLayout` actions + db persistence)_
- [ ] 95. Layout per worktree vs workspace
- [x] 96. Sync panes (broadcast input) _(`ToggleSyncPanes` fans input to all panes in run.rs)_
- [x] 97. Zoom/maximize toggle
- [ ] 98. Swap pane positions
- [x] 99. Layout import/export _(`ExportLayout`/`ImportLayout` actions, JSON round-trip)_
- [x] 100. Auto-layout by terminal size _(responsive sidebar/panel collapse in layout.rs)_

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

- [x] 101. Profiles (work/personal/etc.) _(`profile.rs`: firewalled separate process, reroot; `openspec/specs/profiles`)_
- [x] 102. Per-profile workspaces _(separate state dir per profile → own DB/workspaces)_
- [x] 103. Per-profile config/keybinds/theme _(shared base → per-profile overlay layer)_
- [~] 104. Per-profile proxy keys + budgets _(proxy budgets are per-scope; per-profile key selection via bundle/account)_
- [x] 105. Per-profile credential isolation _(clear-then-allowlist pane env + profile git/gh/gpg identity)_
- [x] 106. Per-profile notification routing _(routing rules + per-profile config overlay; cf. AI 427)_
- [~] 107. Per-profile container network policy _(sandbox config layers per profile; policy depth partial)_
- [x] 108. Profile switcher _(`Action::SwitchProfile` + `palette.rs` profile switcher; launch/focus per-process)_
- [x] 109. Separate state dirs per profile _(reroot: DB/logs/activity/sockets under `profiles/<name>/`)_
- [ ] 110. Profile-scoped audit logs
- [x] 536. Subprofiles — per-subsystem identity/storage split within a profile _(`subsystem.rs`)_
- [x] 537. Subprofile switcher — in-subsystem, in-process rebind _(`subsystem.rs` teardown → bind, no polling)_
- [x] 538. Subsystem abstraction — `workspace`/`comms`/`ai` own storage + cred scope + pane set
- [x] 539. Multi-process model — one window per profile, `flock` singleton, terminal-spawn switcher

### I. Session persistence

- [x] 111. Detach/attach
- [x] 112. Resurrection after reboot
- [x] 113. Restore tree + layouts + pins
- [x] 114. Per-session snapshots _(`session.rs` `persist()` → v6 `tab_groups`/`session_state`)_
- [ ] 115. Named saved sessions _(per-repo `session_name` exists, but no user-named save/load)_
- [x] 116. Auto-save state _(debounced persist in run.rs on meaningful change)_
- [ ] 117. Restore agent state where possible
- [x] 118. Session list/switcher _(palette + sidebar over persisted worktrees/tabs)_
- [ ] 119. Export/import session config
- [ ] 120. Background keep-alive

### J. Remote access

- [x] 121. SSH attach _(terminal-hosts: `ssh` terminal groups exec the connection binary; `openspec/specs/terminal-hosts`)_
- [x] 122. Mosh support _(terminal-hosts: `mosh` connection kind)_
- [~] 123. Tailscale zero-config path
- [ ] 124. iroh embedded p2p — dial by NodeId
- [ ] 125. iroh hole-punching + relay fallback
- [ ] 126. Tunnel stdio agents over iroh/ssh
- [ ] 127. Optional auth-gated web terminal
- [~] 128. Remote daemon mode — agents on remote box
- [~] 129. Local UI → remote agents
- [ ] 130. Mobile client attach (Blink/Termius)
- [ ] 131. QR/NodeID pairing for phone
- [x] 132. Connection status indicator _(remote placement chip in tabbar; sprite connection lifecycle)_
- [~] 133. Reconnect/resume on drop _(sprite tunnel resync + reattach recovery; not general SSH resume)_
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
- [x] 151. System load widget _(cpu/mem/gpu stats cluster in `chrome.rs`, `fit_stats_cluster`)_
- [x] 152. Per-worktree disk widget _(disk size per worktree; cf. 413)_
- [x] 153. Notification badges _(sidebar + panel inbox badges + statusbar `BarBadge::Notifications`)_
- [ ] 154. Now-playing / arbitrary program widget
- [ ] 155. Next calendar event widget
- [~] 156. Remote/network status widget
- [ ] 157. Proxy upstream health widget
- [x] 158. CI/PR check status widget — PR check rollup in the panel; statusbar CI badge via AV 707
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
- [x] 167. Action search by description _(`PaletteMode` All/Files/Content/Git/Symbols in `search_everywhere.rs`)_
- [ ] 168. Palette plugins
- [~] 169. Inline argument prompts
- [~] 170. Palette preview/themes

### N. Theming & appearance

- [x] 171. OKLCH-based theme system
- [x] 172. Light/dark/auto _(`theme.rs` `PRESETS` incl. hand-tuned `light`; live cycle)_
- [x] 173. Custom color schemes _(`[theme.colors]` + `[theme.hues]` TOML overrides, hex RGB)_
- [ ] 174. Per-profile themes _(blocked: profiles (H) not implemented; no override path)_
- [x] 175. Font family/size config
- [x] 176. Nerd Font / icon support
- [x] 177. Border/padding/style config _(`ThemeConfig::pane_padding` + border via `[theme.colors]`)_
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

### R. Agent integration protocols (ACP — the upper control plane)

_Reframed (2026-06-24): superzej is **one control plane in two layers** (see
`docs/superpowers/specs/2026-06-24-acp-two-layer-control-plane-design.md`). The
**lower** plane is `szproxy` (**U/V/W → AR**), which owns model traffic. This
group is the **upper** plane — the **Agent Client Protocol** (ACP), which owns
the agent conversation (sessions, tool calls, permissions, diffs, plans, config
options). ACP is **co-primary**, not "additive/secondary". (The earlier
embedded-`termite-agent`-tile framing is **superseded** — that tile was reverted;
the shipping harness is a **managed `pi`** driven over ACP. See the SUPERSEDED banner
in `docs/superpowers/specs/2026-06-22-embedded-agent-integration-design.md`.)
superzej participates in ACP in **three roles** — **Client** (R1, consume
foreign agents), **Agent** (R2, expose its own harness outward), and **Proxy** (R3,
realize AR for any ACP agent). The two planes meet at two seams:
`providers/set` (point any ACP agent's model traffic at `szproxy`) and
MCP-over-ACP (advertise AR's house tools up to any agent)._

_**Landed (committed on `main`; the real, shipping agent surface):** the R1 client +
the two convergence seams are functionally working against a managed `pi`: ACP client
(`initialize`), client-serviced `terminal/create` (sandboxed run-to-completion),
`fs/read_text_file`, `superzej/edit`+`write` (worktree-scoped), `providers/set`
routing through `szproxy` with a per-worktree minted virtual key, and a
per-worktree `session/update` → statusbar **agent chip** (tool + ctx% + connection
lifecycle). Code: `crates/superzej-svc/src/acp/` (`AcpClient`),
`crates/superzej-core/src/acp/` (data model), `extensions/superzej-acp.ts` (the
in-pane pi bridge, pinned pi `0.80.2`), `crates/superzej-host/src/{bouncer,relay}.rs`
(sealed-sandbox tool gate + model relay). Transport is **TCP or a bind-mounted unix
socket + newline-JSON** (the pi extension's server), not stdio. (This work was earlier
mis-recorded as an uncommitted `sz/spicy-dragon` branch; it is committed on `main`.)_

_**Minimal-surface UX (original stance, being revisited):** the shipped surface is
deliberately MINIMAL — pi's terminal pane is the conversation and the chip is the only
native reflection, so 233/234, a `Section::Agent` panel, a fleet view, and
`session/prompt` steering were scoped as **intentional non-goals** (agent edits
AUTO-APPLY, so a review gate is not required for correctness). **The "finish ACP
surfaces" track now takes up 232 (4-option permission gate + remember-choice), 233
(read-only diff view of agent edits), and 230 (session lifecycle)** as additive
surfaces on top of the minimal baseline — see the approved plan._

**R1 · ACP Client — consume foreign harnesses:**

- [~] 229. ACP client core — `initialize` + capability negotiation (protocolVersion; advertise `clientCapabilities` fs+terminal+`clientInfo`; parse `agentCapabilities`/`promptCapabilities`/`mcpCapabilities`/`authMethods`/`agentInfo`); `authenticate`/`logout`
- [ ] 230. ACP session lifecycle — `session/new`, `session/load`, `session/resume` (reconnect, no replay), `session/list`, `session/close`, `session/delete`, `session/fork`, `session_info_update`; map to worktree-tabs + session resurrection + time-travel-replay
- [~] 231. ACP streaming updates — full `session/update` set: `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `usage_update`, `config_option_update`; StopReason handling + `session/cancel`
- [~] 232. ACP permission requests → UI — `session/request_permission` (allow_once/allow_always/reject_once/reject_always → optionId|cancelled), remember-choice persistence, native overlay. Host-intercept gate: `bouncer.rs` `gated_kind`/`ApprovalQueue` + `menu.rs` overlay gates shell/edit/write on the sealed path. The 4-option model now landed (`bouncer::ApprovalDecision` = AllowOnce/AllowAlways/RejectOnce/RejectAlways; `approval_menu` offers allow-once/allow-session/deny-once/deny-session; Esc = deny-once) + session-scoped remember-choice (an "always" pick records `(worktree, kind)→allow` in a loop-owned map the off-loop gate task consults to auto-resolve without re-prompting). Gaps: durable-across-restart persistence (deliberately session-scoped for now, matching ACP `allow_always` semantics; DB-backing deferred to avoid growing the pinned `db.rs`); pi 0.80.2 emits no `session/request_permission`, so the gate is host-driven off the intercepted tool calls, not a native ACP permission request
- [ ] 233. ACP diff rendering — `tool_call` diff content (`oldText`/`newText`/`path`) into the existing diff/review pane (T 260)
- [ ] 234. ACP plan/tool-call events — tool kinds (read/edit/delete/move/search/execute/think/fetch/other), status (pending/in_progress/completed/failed), `locations` (path+line) → sidebar/editor follow-along
- [ ] 235. ACP Registry integration — fetch `registry.json`, parse `agent.json` manifest + icon, one-command install/launch of authenticated agents
- [ ] 684. Session Config Options surfacing — render `configOptions` (model/mode/thought_level selectors) in palette/statusbar; `session/set_config_option` + `config_option_update` (supersedes Session Modes)
- [ ] 685. `usage_update` consumption — context-window `used`/`size` + optional `cost{amount,currency}` per session → feeds S 246/249/250 and V 289/290 spend attribution
- [ ] 686. ACP Elicitation — `elicitation/create` form mode (restricted JSON Schema) + URL mode (OAuth); accept/decline/cancel → native iocraft form UI (shares AL 459)
- [~] 687. Client filesystem surface — serve `fs/read_text_file`/`fs/write_text_file`, unsaved-buffer aware, scoped to the worktree
- [~] 688. Client terminal surface — serve `terminal/create`/`output`/`wait_for_exit`/`kill`/`release` (env/cwd/outputByteLimit) through our PTY + `sandbox::enter_argv`; embed in tool calls. _We are a terminal multiplexer — this makes us a premier ACP terminal client._
- [~] 689. **Configurable LLM Providers** — `providers/list`/`providers/set`/`providers/disable` (id/apiType/baseUrl/headers) to route any ACP agent's model traffic through `szproxy`. **The R↔U bridge** _(connects U 271/287; powers U 283 local upstreams)_
- [ ] 690. Agent Telemetry Export — inject `OTEL_EXPORTER_OTLP_ENDPOINT` + `params._meta` traceparent into agent subprocs; ingest into the perf/observability suite _(feeds S 254)_
- [ ] 691. Protocol-version negotiation + `_meta`/extensibility + **v2 readiness** — track the ACP v2 redesign (unified `capabilities`, object-valued markers, item-based `plan_update`, upsert `tool_call`, content chunks) and build v2-shaped

**R2 · ACP Agent — expose superzej / termite outward:**

- [ ] 692. termite-agent as an ACP **agent server** — implement the Agent side so termite is consumable by Zed/other ACP clients; submit to the ACP Registry (distribution play) _(wraps the same `AgentRuntime` as `apps/agent.rs`)_
- [ ] 693. Emit ACP updates from termite — `plan`/`tool_call`/`tool_call_update`/`usage_update`/`config_option_update` over the ACP channel
- [ ] 694. superzej house-tools as an ACP agent endpoint — expose house tools/context (rtk, sem, weave) as an ACP agent for foreign clients

**R3 · ACP Proxy — the convergence (AR realized over ACP):**

- [ ] 695. AR gateway as an **ACP proxy** — `proxy/initialize` + `proxy/successor` + conductor so capability injection / prompt layering / tool filtering (AR 541–551) work with **any** ACP agent, not just termite _(upper-layer twin of AR; subsumes AGENTS.md/hooks/plugins)_
- [~] 696. **MCP-over-ACP** — expose the central MCP registry over the ACP channel (`mcp/connect`/`mcp/message`/`mcp/disconnect`, `mcpCapabilities.acp`) with brokered creds, no open ports _(transport for AL 455 / AR 541–543)_

**Native adapters — fallback for non-ACP harnesses (ACP-registry-first):**

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
rich token/tool telemetry remains Phase 3 once proxy/adapters exist. For ACP
agents the telemetry arrives over standard paths: context-fill (246) and live
tokens/cost (249/250) from `usage_update` (**R 685**); OTEL ingestion (254) from
Agent Telemetry Export (**R 690**)._

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
- [x] 256. Needs-attention surfacing _(attention tiers + default Attention sort + reason hint + statusbar ✋ chip — openspec `add-sidebar-attention-sort`; authoritative OSC/CLI signals remain `add-osc-attention-signaling`)_
- [ ] 257. Transcript viewer
- [ ] 258. Session replay
- [ ] 655. Per-worktree status/checkpoint field — agent-writable free-text "what just happened / status / next step" string (set via a CLI verb, `--json`), surfaced in the sidebar (feeds B 28), statusbar, panel, and the attention queue; read-before-write to preserve context (Orca worktree-comment pattern)

### T. Agent review & merge

_Tier-1 attention routing keeps the existing one-key jump and review/merge flows
as the agent-specific side of the broader attention queue._

- [x] 259. Needs-attention jump (one key) _(`Alt a` / `attention-next`, wraps the needs-you set, crosses workspaces — openspec `add-sidebar-attention-sort`)_
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
- [~] 758. Agent-driven merge-queue driver — assign worktree branches to the queue (`merge add [--all]`) and drain them serially (`merge drain`): clean branches auto-land; a conflict or red gate dispatches a headless CLI agent (Claude Code default, any command) inside the branch's worktree to rebase/resolve/fix, then re-folds and auto-lands (configurable), marking `needs_human` after `agent_max_attempts`. Agent never touches the target — superzej does the object-DB fold + CAS, preserving the merge-guard coherence guarantee. Builds on the fold-actor (`4fbc92b`); `add-agent-merge-driver`. Follow-up: in-TUI actions + `auto_drain` (blocked on a run.rs/keymap.rs extraction). _(complements 263/268 and orchestration core Q)_

### U. LLM proxy

_The **lower control plane**. Foreign ACP agents are pointed here via ACP's
Configurable LLM Providers (`providers/set` with `baseUrl` = `szproxy`) — **R 689**
is the bridge; 283 (local upstreams) is reachable the same way. Per-agent virtual
keys (287) are the `providers/set` credential target._

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
- [x] 281. Model/tier aliasing _(routes-doc `aliases` map: client model id → route)_
- [~] 282. Auto-downgrade under pressure
- [ ] 283. Local model upstreams (Ollama/vLLM)
- [ ] 284. Prompt-cache preservation (native Anthropic path)
- [x] 285. Streaming passthrough (no buffering)
- [~] 286. Tool-call field preservation
- [x] 287. Per-agent virtual keys _(worktree/workspace/agent/zone-scoped keys, upstream account pinning, per-identity budgets; `superzej proxy keys`)_
- [x] 288. Proxy managed as daemon/pinned program _(host auto-launch)_
- [~] 656. Interactive per-agent account/credential switcher — status-bar chip to hot-swap which subscription/account (or virtual key) a harness uses without re-auth; UX layer over the proxy's key load-balancing (280) + per-agent virtual keys (287); running sessions keep their account until restart (Orca hot-swap)

### V. Cost / limit / budget

_Proxy-side spend (289/290) gains ACP parity: foreign agents report their own
context size + cost via `usage_update` (consumed by **R 685**, emitted by termite
via **R 693**), reconciled against proxy-measured spend._

- [x] 289. Per-request cost logging
- [x] 290. Spend attribution — agent/worktree/workspace
- [ ] 291. Spend-mode vs subscription-mode accounting
- [x] 292. Budget caps ($/tokens) per scope
- [x] 293. Enforce caps (refuse/downgrade) _(refuse-on-breach)_
- [ ] 294. RPM/TPM rate limiting
- [x] 295. Daily/weekly/monthly ceilings _(rolling windows: anchor advance on rollover + window-aware checks)_
- [x] 296. Kill-switch on breach _(`superzej proxy budget kill` + enforcement)_
- [ ] 297. Cache-hit-ratio tracking
- [x] 298. Spend history + export _(audit rows incl. duration/TTFB; `superzej proxy stats --json`)_
- [x] 299. Cost dashboards/charts _(TUI proxy dashboard `Ctrl Alt l` + `/stats` endpoint + CLI: spend, tokens/sec, p50/p95, per-backend/route/scope)_
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

- [x] 309. sem-core integration (entity parsing) _(`semantic.rs` `parse_entities` — tree-sitter grammar+query, `Entity`/`EntityKind`/`Lang`)_
- [x] 310. tokei LOC/language counts _(`core/loc.rs` + `host/loc_scan.rs`, DB v31 `loc_cache`)_
- [x] 311. Entity-level diffs _(`semantic.rs` `entities_for_diff`, `EntityChange`/`Touch`)_
- [x] 312. Entity-level blame _(`semantic.rs` `blame_entities`, `EntityBlame`)_
- [~] 313. Impact/blast-radius analysis _(`semantic.rs` `impact_summary` = per-file entity-count aggregation only; persistent call/reference graph + true blast radius is openspec `add-semantic-blast-radius`)_
- [ ] 314. weave merge driver (code-only default)
- [ ] 315. Entity-claiming for multi-agent coordination
- [ ] 316. inspect risk scoring
- [x] 317. Entity-derived commit messages _(`semantic.rs` `derive_commit_message`, structural/no-AI; consumed in `hydrate.rs`)_
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
- [x] 328. Commit signing _(GPG signing args plumbed through commit/cherry/revert; commit overlay `^S` cycles inherit→sign→no-sign)_
- [x] 329. Hooks-aware (pre-commit) _(commit overlay `^N` toggles `--no-verify`; rejected hooks fold stdout+stderr into a `HookFailed` popup, terse git refusals stay on the status line)_
- [x] 330. Cherry-pick/revert _(+ continue/skip/abort)_
- [x] 601. Word-level / intra-line diff highlighting (base vs working copy) _(`diff_highlight::word_diff` + `changed_mask` drive a brighter run-level tint in `diff_cell`; coverage-gated tests in core)_
- [x] 602. Center-gutter visual hunk stage/revert — "`git add -p`, made visual" _(`panel/staging.rs` `hunk_revert_indices`; `R` → `GitMsg::RevertHunk` → confirmed `DiscardLines` over the cursor hunk)_
- [x] 604. Rollback/discard window — checkbox tree of changes, optional delete of added files, per-row diff _(`panel/rollback.rs` modal: space/a/c marking, per-row hunk preview, `delete` badge for untracked; Enter enqueues one `DiscardFiles` batch partitioning restore vs delete; palette command `rollback`)_
- [x] 605. Plain push/pull/fetch when ahead/behind upstream (non-PR fast path) _(`Action::Push`/`Pull`/`Fetch` enqueue directly via `enqueue_git_op`; palette commands `git-push`/`git-pull`/`git-fetch`)_

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
- [x] 333. PR review comments _(`pr_view.rs` `PrConversation`/`ReviewState`, inline/line comments; openspec `add-inapp-pr-view`, archived)_
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
- [x] 358. devcontainer.json support _(full spec: `core::devcontainer` JSONC parser + `devcontainer_overlay` fold onto `[sandbox]`; image + `build.dockerfile` (`sandbox_build`, synchronous build in `ensure`) + `dockerComposeFile`+service+runServices (`sandbox_compose`, pane enters via `docker compose exec`) + `remote/containerEnv` + mounts/forwardPorts; full lifecycle (initialize→prepare, onCreate/updateContent/postCreate→one-time Exec, postStart/postAttach→init_script); native OCI `features` resolver (`devcontainer_features`: overrideFeatureInstallOrder, option→env, oras|curl pull + install.sh). Every category trust-gated via `devcontainer.*` GatedRequests through the existing repo_trust flow; wired into `resolve_env_trusted` + `host_provision`. E2E-verified against live podman/docker (`tests/devcontainer_e2e.rs`, `PODMAN_E2E_FORCE=1`): build→run→exec, image+env+lifecycle, compose up+exec. Limitations: features live-pull not E2E'd (needs ghcr network); lifecycle/features assume the host-provision OCI path; compose service mounts/ports come from the compose file.)_
- [x] 359. Nix devshell per worktree _(sandbox devshell injection + direnv warm; `openspec/specs/sandbox`)_
- [~] 360. Ephemeral reset between runs
- [x] 361. Container↔worktree binding
- [x] 362. Default-on with --no-sandbox escape

### AC. Container networking

- [~] 363. Per-container firewall
- [~] 364. Egress presets — offline/proxy-only/full
- [ ] 365. Container DNS proxy
- [ ] 366. Single auditable egress point
- [ ] 367. Shared chokepoint with LLM proxy
- [x] 368. Open-port detection — `[forward]` off-loop detector (`ss` probe + diff)
- [x] 369. One-click open in browser — System ▸ Forward panel, `o` opens preview
- [ ] 370. Friendly local hostnames (worktree.localhost)
- [x] 371. Reverse proxy for ports — userspace `exec`-bridge proxy, auto-remap on conflict (local; ssh/k8s = Phase 2/3)
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
- [x] 386. Prewarmed pool (fast spawn) _(`[lifecycle.pool]` warm-spare sandbox pool, DB v26)_
- [ ] 387. Intelligent resource caching (node/cargo/pip)
- [x] 388. Shared cache across worktrees _(`[disk].sccache` → `RUSTC_WRAPPER`/`SCCACHE_DIR` + `[disk].shared_target_dir` → `CARGO_TARGET_DIR` injected into pane env at `agent::launch_spec`; shared-target serializes builds, opt-in)_
- [x] 389. Auto cache cleanup _(see 48: PR-merge/close auto `cargo clean`; manual `superzej clean [--all]`)_
- [ ] 390. Snapshot/checkpoint (CRIU/commit)
- [ ] 391. Rollback container state
- [ ] 392. Image build cache
- [ ] 393. GPU passthrough
- [ ] 394. Base image catalog/templates
- [x] 749. Commodity-VPS provider backend — Hetzner via native REST (`Provider::Vps`, ssh exec/files shim, `szhost vps-ssh` self-bridge, intent-ledger + label-scoped reaper, `superzej env image-bake`; DO/Vultr adapters + firewall/spend-UI follow) _(openspec `add-vps-providers`; not live-verified)_
- [x] 756. DigitalOcean + Fly.io provider backends — DO as a second `VpsKind` behind a `VpsShaper` trait (shared ledger/reaper/self-bridge/bake); Fly as a CLI-free `Provider::Fly` (Machines REST + GraphQL IPv4 + guest sshd over the managed keypair, scale-to-zero, baked `nix/fly-sandbox-image.nix`, `fly_reaper`) _(openspec `add-do-fly-providers`; DO+Hetzner+Fly-control-plane live-verified)_
- [x] 757. Self-serve environment setup UX — layered secret store (`secret.rs`: `keyring:`/`env:`/`file:`, keyring→file writer) + comment-preserving config write path (`config_write.rs`, `toml_edit`) + CLI (`env create`/`rm`/`test`, `config set`) + TUI (`env_wizard` modal + System ▸ Environments panel, unified `‹ local › ssh fly digitalocean hetzner daytona`) _(openspec `add-env-setup-ux`)_

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
- [x] 606. File management from the tree — new/rename/delete (with confirm) + file-type icons via the yazi drawer; git/VCS-status colors via the vendored `git.yazi` plugin, seeded + registered by `yazi.rs::apply_git_status_policy` (`[drawer] git_status`, default on) with `[git]` theme hues _(live color render pending a real-terminal check)_

### AG. Editor integration

- [x] 405. Open in $EDITOR (helix)
- [x] 406. Open in split/new tab
- [ ] 407. GUI editor handoff
- [x] 408. Jump to file:line from logs/diffs _(`panel_util.rs` `parse_file_line`, `+N` syntax)_
- [~] 409. Editor as pinned tile _(opens as a floating tool, not a true pin)_
- [x] 410. Per-workspace editor override _(`tool_command("editor")` per-workspace resolution)_

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
- [x] 420. Rules engine — event→action _(user-defined routing rules; `openspec/specs/notifications`)_
- [x] 421. Desktop notifications _(via `notify-send`, gated by `desktop_min_urgency`; not the notify-rust crate)_
- [ ] 422. Push to phone (ntfy)
- [ ] 423. Push to phone (Telegram)
- [~] 424. Per-event opt-in _(urgency-threshold gating, not yet per-event)_
- [x] 425. Contextual tree dots _(activity-dot state machine)_
- [x] 426. Do-not-disturb / quiet hours _(DND config + scheduled quiet-hours gating)_
- [x] 427. Per-profile routing _(routing overlay merges global/profile/workspace)_
- [x] 428. Notification history/center _(notifications panel section + inbox, Enter-to-expand)_
- [x] 429. Sound/bell config _(bell/command/off modes, per-priority gates)_
- [x] 430. Aggregated bus across all sources _(core `EventBus` aggregates PR/agent/test/log/worktree/process events)_

_**Near-term (flagged 2026-06-29): sophisticated in-app diagnostic / toast surfacing.**
Today `msg::warn`/`msg::error` + `config_warn` route through `tracing` to the LOG
FILE once the subscriber is installed — invisible in the TUI. So config/overlay
parse errors, capability downgrades, and best-effort provisioning failures sink
silently. Motivating bug: a malformed repo `.superzej.toml` silently dropped an
`env = "sprites"` selection and the session ran local; only `env_halt_reason` now
halts (warning modal) on the placement-changing case. The general fix is a
first-class surfacing layer on top of the existing `toast` module + `EventBus` +
notifications inbox (428)._

- [ ] 749. In-app diagnostic surfacing — tee `msg::warn`/`error` + `config_warn` + parse/halt diagnostics to visible toasts (not just the log file); a `tracing` layer or a diagnostics sink the loop drains + renders
- [ ] 750. Toast severity + lifecycle — info/success/warn/error tiers, per-severity TTL + sticky errors, dedup/coalescing of repeated diagnostics, bounded stacking with overflow into the inbox
- [ ] 751. Actionable toasts — keybound/click-through actions (open the offending file, open the log, retry the halted env, copy the error), wired to the palette + EventBus
- [ ] 752. Unified diagnostics center — fold config + runtime diagnostics into the notifications inbox (428) with source attribution + severity/source filtering + replay
- [ ] 753. Startup diagnostics gate — a visible startup summary whenever config/overlay parse errors or capability/placement downgrades occurred, so a dropped overlay or degraded remote→host is never silent (consistent with the failover-halt modal)

### AJ. Security / opsec

- [ ] 431. Credential brokerage — agents never see raw keys
- [~] 432. Scoped capability tokens per agent _(`grants.rs` capability-grants foundation; `openspec/specs/capability-grants`)_
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
- [ ] 754. Sandbox `env_overrides` off the cmdline — exports ride the wrapper script (an argv element, /proc-visible); move to a 0600 env-file transport sourced by the wrapper (local bwrap `--setenv` passthrough already fixed)
- [ ] 755. OCI env passthrough off the cmdline — `podman/docker create -e K=V` leaks values; use value-less `-e KEY` (inherit from client env)

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
- [~] 454. Headless CLI over the API _(headless seams landing CLI-first: `wt new`/`rm`, `open --no-launch`, blanket `--json` + exit-code contract — `openspec/changes/add-cli-namespaces-and-remote-open/`)_

### AL. MCP server

_Exposure transport: in addition to stdio/HTTP, the MCP server is advertised over
the **ACP channel** via MCP-over-ACP (**R 696** — `mcp/connect`/`message`/
`disconnect`, `mcpCapabilities.acp`), so foreign ACP agents get house tools with
brokered creds and no open ports. This is what lets AR 541–543 reach any harness._

- [x] 455. MCP server over core
- [x] 456. Tools (action verbs)
- [x] 457. Resources (task://, fleet://)
- [ ] 458. Prompts (templates)
- [ ] 459. Elicitation (approve/answer flow)
- [ ] 460. Sampling (borrow client model)
- [x] 461. spawn_subtask (recursive)
- [ ] 462. get_sibling_state / wait_for_task
- [ ] 463. Shared blackboard resource
- [x] 464. check_my_budget
- [x] 465. request_human escalation
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
- [x] 476. Music tile (rmpc/ncmpcpp) _(optional `[media]` feature, off by default: `core::media::MediaState`, MPRIS/mpv-IPC/playerctl backends in `svc::media`, panel `Section::Media`, statusbar badge, Alt-m transport binds + playlist/player pickers)_
- [~] 477. Files tile (yazi/lf)
- [ ] 478. Cross-tile actions — email→task, agent→Matrix
- [ ] 479. Unified comms inbox
- [ ] 480. Workspace presets — comms/dev/personal
- [ ] 540. Comms as a subprofile-aware subsystem — per-subprofile accounts/storage/creds (first consumer of H. 536–538; design `docs/superpowers/specs/2026-06-11-profiles-subprofiles-design.md`)

### AN. Audit / logging / replay

- [~] 481. Central event log (all sources)
- [x] 482. Per-task replay — time-travel recording + scrub/search overlay (`Alt+r`) +
       vim registers + replay-subsumed screen swap (`openspec/specs/time-travel`)
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
- [x] 517. Test status rollups — pass/fail/running state in panel, sidebar, and statusbar _(panel Tests section; sidebar alert badge on failures; statusbar `tests` widget `✓pass ✗fail` in `chrome.rs::bottombar_widget`)_
- [~] 518. Run/debug selected test — selected (`r`) / failed (`f`) / all (`R`) / file (`F`) / package (`p`) scopes in `test_task_for_run` (path for pytest/jest/vitest, module fallback for cargo/go); _cursor-relative "nearest" deferred (viewer hands editing to `$EDITOR`, no in-process cursor — folds into file scope) and DAP debug-run is Tier-2 (525–528)_
- [x] 519. Problems / diagnostics panel — compiler/linter/config/LSP diagnostics with file:line jumps _(`panel/sections/problems.rs`)_
- [x] 520. Named task registry — `[[tasks]]` (explicit config) + discovered providers (just, cargo, npm, etc.) and aliases _(`panel/sections/tasks.rs` + `task.rs` discovery)_
- [x] 521. Task lifecycle controls — run/stop/restart/rerun from palette/panel/keybinds for any task
- [x] 522. Task output capture + problem matching — feed Tests and Problems without polling _(`task.rs` parses rustc/gcc/clang → diagnostics)_
- [x] 523. Search Everywhere provider aggregation — actions, files, symbols, tasks, tests, problems, git, worktrees _(`PaletteMode` prefixes `!`/`$`/`%` for tasks/problems/tests; filled synchronously from in-memory panel state alongside the async file/content/git/symbol workers)_
- [x] 524. Non-agent process attention routing — exited/failed/waiting panes join the attention queue _(`ProcessExited` event + exit classification/policy)_
- [ ] 525. DAP client substrate — debug adapter JSON-RPC service seam in `superzej-svc`
- [ ] 526. Debug breakpoints and stepping — continue/pause/step controls and breakpoint state
- [ ] 527. Debug variables/watch/call-stack panel — inspect runtime state in the right panel
- [ ] 528. Debug launch/attach configurations — task-backed debug profiles per workspace
- [~] 529. LSP client substrate — language-server JSON-RPC service seam in `superzej-svc`
- [ ] 530. Go-to-definition and find-references — navigate via `$EDITOR`/panel handoff, not in-place editing
- [x] 531. Document/workspace symbols — feed Search Everywhere and outline/reference views _(`panel/sections/symbols.rs` + LSP/tree-sitter)_
- [x] 532. Hover/signature/code-action preview — read-only context and previewable actions
- [ ] 533. Per-worktree local timeline — git/files/tasks/tests/agents/checks activity history
- [ ] 534. Restore/compare from local timeline — inspect or recover local snapshots where available
- [ ] 535. Unified layout+task template — native `CenterTree` layout + tasks + pins + sandbox preset

### AR. AI gateway / context fabric

_The proxy track (**U/V/W**) graduating into an **AI gateway / context fabric**: the
proxy becomes the **AI control plane** — one interception point all model traffic
crosses, so any cross-cutting concern is **configured once and inherited by every
harness**, translated to each harness's format. The capability-injection items
(541–551) have **two realizations**: in-process for the embedded termite harness,
and — for **foreign** ACP agents — over the upper plane as an **ACP proxy**
(**R 695**) with house tools delivered via MCP-over-ACP (**R 696**). Appended at the end with the other
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

### AU. Environment bundles (.env / dotfiles / profiles)

Design approved (2026-06-22): `docs/superpowers/specs/2026-06-22-env-bundles-design.md`.
The **soft middle** between per-agent account switching (656) and the heavyweight
process-profile firewall (H 101–110): named **bundles** of env vars + credential/config-
dir redirection + dotfiles + per-provider account selection, **bound at any scope**
(global/workspace/worktree) and injected at the pane-spawn seam — so "work vs personal"
differs _within one process_. Generalizes `account.rs` (becomes a bundle consumer);
AI-free track. Locked: **(1)** lighter complement, not a firewall replacement; **(2)**
three dotfile tiers (config-dir redirect default / materialized dotfiles / synthetic
HOME); **(3)** named bundles **+** opt-in allowlisted `.env`; **(4)** `env:` + pluggable
secret resolvers, never persisted. Closes the `spawn_with_env` inherit-everything leak
(shared with H) and fills item 38 + the env-restore half of 54/657.

_Phases A–C landed (`bundle.rs`, `openspec/specs/env-bundles`); Tier-2/3 dotfile
materialization (Phase D) is partial._

- [x] 735. `env::compose()` + `ResolvedEnv` — single resolution seam returning overrides/block/mounts (Phase A) _(`bundle::compose`)_
- [x] 736. Bundle config schema — `[bundle.<name>]` (env/accounts/config_dirs/dotfiles/home/dotenv/extends) + `[workspace.<slug>].env_bundle` (Phase A)
- [x] 737. Per-scope bundle bindings — `bundle:[ws:|wt:]` precedence over `ui_state` (worktree → workspace cfg → workspace ptr → global) (Phase A)
- [x] 738. Tier-1 config-dir redirection — `CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`GIT_CONFIG_GLOBAL`/`GH_CONFIG_DIR`/`GNUPGHOME`, no file ops; the implicit default tier (Phase A)
- [x] 739. Shell-pane wiring — **every** pane spawn (agent _and_ plain shell) routed through `env::compose`, so shells inherit the bundle identity (Phase A)
- [x] 740. Clear-then-allowlist base env in `spawn_with_env` — curated base + bundle on top; closes the inherit-everything cred leak (shared with H) (Phase A)
- [x] 741. `account.rs` becomes a bundle consumer — account selection is a bundle field; precedence helpers lifted to bundle scopes (Phase A)
- [x] 742. Pluggable secret resolvers — resolved off-loop at launch, never persisted, graceful degrade (Phase B) _(env-bundles spec: "Secrets resolve at launch")_
- [x] 743. Opt-in `.env` loading — gated by `dotenv = true` + per-path content-hash allowlist in `ui_state` (Phase C)
- [x] 744. `.env` security boundary — low precedence (never overrides bundle creds) + credential-shaped-key filter (`*_TOKEN`/`*_KEY`/`*_SECRET`/`*_PASSWORD`) (Phase C)
- [~] 745. Tier-2 materialized dotfiles — symlink/template a source tree into a managed per-bundle HOME; idempotent, off the event loop (Phase D) _(bundle `dotfiles` field present; local materialization depth partial)_
- [~] 746. Tier-3 synthetic HOME — `home = "managed"` roots panes at the bundle HOME; path-preserving sandbox mount (Phase D) _(bundle `home` field present; full synthetic-HOME rooting partial)_
- [x] 747. Bundle switcher UI — `Action::SwitchBundle` + `palette.rs` bundle switcher bind the active bundle at worktree/workspace/global scope (Phase E)
- [~] 748. Multiple Claude profiles (worked example) — `work`/`personal` bundles selecting `accounts.claude` + git identity + proxy endpoint (consumes 735–747; ties 656, AR virtual keys 287) _(building blocks all present; documented worked example pending)_

### AV. CI/CD inspection (cross-provider pipelines, runs, jobs, logs)

_A CI/CD insight layer (inspired by `termkit/gama`): turns the GitHub-only PR check
rollup (Z 332) into **run history, job/step drilldown, log viewing with jump-to-failure,
and trigger/rerun/cancel** across providers. The `CiProvider` trait is a **sibling** of
the AT forge trait (631), not a subset — CI is a different axis: GitHub/GitLab/Gitea/
Forgejo are forge **and** CI, but Drone/Woodpecker/Jenkins/Argo/`act` are CI-only. A
provider-agnostic run→job→step→log model lives in core; providers degrade native-API →
CLI → unavailable. Surfaced as a panel `Section::Ci` rollup **and** a full-screen
drilldown (Runs → Jobs/Steps → Logs). **AI-free** — "why did it fail" is log + jump-to-
failure, no LLM. Folds in Z 332 and L 158. Validated on GitHub + GitLab first._

- [x] 698. `CiProvider` trait + normalized model — `runs`/`run_detail`/`logs`/`workflows`/`trigger`/`rerun`/`cancel`/`capabilities`; `CiRun`→`CiJob`→`CiStep` + `CiLog`/`CiWorkflow` in `superzej-core/src/ci.rs` (+ `CiState` mappers, log failure-scanner, CI-config detection); trait in `superzej-svc/src/ci.rs` w/ native+CLI degradation, capability-gated mutations (Phase A) ✓
- [x] 699. `ci_runs_cache` table + `[ci]` config — TTL'd JSON cache (mirrors `pr_cache`, db v18), `config_enum!` `CiProviderKind` + per-provider sub-tables (gitlab/drone/woodpecker/jenkins/argo) w/ `env:` tokens, poll interval, live-refresh default, log-tail lines (Phase A) ✓
- [x] 700. GitHub Actions provider — `gh run list`/`gh run view --json jobs`/`gh run view --log`; run history, jobs/steps, logs; reuses `gh` auth; fixture-tested parsers; deepens Z 332 (Phase A) ✓
- [x] 701. GitLab CI provider — pipelines→jobs→trace via `glab api`; subgroup-aware project path; fixture-tested parsers (Phase A; also AT 633) ✓
- [x] 702. Panel `Section::Ci` — Work-tab rollup: recent runs + per-run state glyph + duration, latest run's jobs when deep; summary chip (✓N ✗N ●N) (Phase A) ✓
- [~] 703. CI drilldown view — `szhost ci view <id>` (run→jobs/steps) + `ci log` + the deep/Full panel section serve the Runs→Jobs→Logs drilldown today; a dedicated full-screen center-pane overlay (live-refresh toggle, filter) is the remaining UI iteration (needs live-terminal verification) (Phase A)
- [x] 704. `RefreshKind::Ci` + `spawn_ci_cache_refresh` — off-loop poller (`spawn_blocking` + mpsc + `TerminalWaker`), on-switch + PR-cadence interval; writes `ci_runs_cache`; 0% idle preserved (Phase A) ✓
- [x] 705. CI actions + keymap + palette + CLI — `Action::OpenCi` (+ `ACTION_SPECS`, `palette:true`); full `szhost ci` group: `runs`/`view`/`log`/`rerun`/`trigger`/`cancel`/`detect`; smoke-tested (Phase A) ✓
- [x] 706. "Why did it fail" — `ci log` applies the `log_tail` cap and prints a `>> first failure at line N` marker via `CiLog::first_failure_line` (`##[error]`/error/exit-code/panic scan, no AI) (Phase A) ✓
- [x] 707. Statusbar CI badge — closes L 158: red `✗N CI` chip on failures, amber `●N CI` while running, silent when green (Phase A) ✓
- [ ] 708. Trigger / `workflow_dispatch` — dispatch a workflow with declared inputs (gama's headline; extended-inputs JSON for 10+ inputs); capability-gated (Phase B)
- [ ] 709. Cancel + rerun across the trait — rerun all/failed/single-job, cancel a run; rerun-failed already exists for GitHub (Z 332) (Phase B)
- [ ] 710. Live-refresh toggle — gama's `ctrl+l`; bounded-CPU polling while the view is open, configurable interval (Phase B)
- [ ] 711. Gitea/Forgejo Actions provider — Gitea/Forgejo API / `tea`; GitHub-compatible-ish Actions (Phase C; also AT 634/635)
- [ ] 712. Drone provider — Drone API + token, per-instance server URL; promote/restart (Phase D)
- [ ] 713. Woodpecker provider — Woodpecker API (Drone fork); restart (Phase D)
- [ ] 714. Jenkins provider — Jenkins JSON API + crumb, per-instance URL / basic-auth or token; build with params (Phase D)
- [ ] 715. Argo provider — Argo Workflows (k8s / `argo` CLI) + Argo CD (`argocd` API); submit/resubmit/sync; k8s-context dependent (Phase D)
- [ ] 716. Local `act` runner — run `.github/workflows` locally via `act`; stream logs into the run view (Phase E)
- [ ] 717. Repo-health / CI-config detection — which CI files a worktree has, recent pass-rate, currently-running count; surfaced in the CI view header (Phase E)

### AW. Log Analyzer (sz-log)

_A native, zero-IPC structured log viewer providing `hl`-like capabilities for worktree files, containers, and tasks. Integrates heavily with the render plan to ensure high-throughput log streams do not violate the 0% idle / <16ms frame invariants._

- [x] 718. `LogProvider` trait + bounded ring-buffer memory model
- [x] 719. Zero-copy JSON & logfmt parsers (envelope extraction)
- [~] 720. Off-thread log ingestion worker + batching waker (wake-storm prevention)
- [ ] 721. Full-screen center-pane log overlay UI
- [x] 722. Filter DSL — fuzzy text, severity normalization, exact field matching
- [ ] 723. Dynamic field projection — hide/show/reorder JSON keys
- [ ] 724. Tailing vs Paused mode — auto-pause on scroll
- [~] 725. File tailing provider (`notify` backend)
- [ ] 726. Container tailing provider (resolves AD 383)
- [ ] 727. Editor handoff — jump to `file:line` from stacktraces (resolves AG 408)
- [ ] 728. Field Explorer drawer — surface schema/keys dynamically based on current view

### AX. Native Windows Support

_The Windows-native workspace shell (AI-free by default), bypassing WSL/MSYS2 for a native sub-300ms, zero-IPC experience. Core features (multiplexing, rendering, git) already map cleanly to Windows thanks to the `portable-pty`/`termwiz` foundation._

- [ ] 729. Cross-platform filesystem watching — replace `inotify` with `notify` (`ReadDirectoryChangesW`) for diff watchers
- [ ] 730. Native Sandboxing: AppContainers — low-integrity process isolation granting read/write ACLs only to the specific worktree path
- [ ] 731. Native Sandboxing: Job Objects — prevent fork-bombs, block UI popups, and ensure child process trees die instantly on tab close
- [ ] 732. Standardized paths — migrate from Unix `$XDG_STATE_HOME` to `directories` crate resolving to `%LOCALAPPDATA%\superzej`
- [ ] 733. Signals mapping — map Unix profiling triggers (`SIGUSR2`) to internal keymaps or named events for Windows flame-graphs
- [ ] 734. PowerShell / NuShell defaults — default pane spawning to native Windows shells over `cmd.exe`

### AI-free mode (audience-widener)

- [~] 511. AI-free mode — run as a pure terminal workspace/worktree manager, no agents/proxy/LLM
- [~] 512. All features usable manually — git, worktrees, containers, pins, comms tiles, monitoring with zero AI
- [ ] 513. Compile-out AI components — feature flag for a lean binary without proxy/agent/MCP layers
- [~] 514. Graceful degradation — AI panels, dots, cost widgets simply absent; nothing else breaks
- [x] 515. No-AI privacy posture — zero outbound model traffic, smaller attack surface, fully local
