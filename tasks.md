# superzej — roadmap & progress

503 features across 42 groups (A–AP). The list is really **two tracks joined by
one keystone**: an AI-free _shell_ track and an AI track, bridged by the **proxy**.
That shape drives the phasing below. Original numbering is preserved; gaps are
deliberate cuts (499, 500, 502, 505, 506, 507, 510 dropped from the moonshot set;
web dashboard 510 and voice 499 already cut).

**Status legend:** `[x]` done · `[~]` in progress / partial · `[ ]` not started.
Per-feature statuses below are verified against the current codebase. See
`CLAUDE.md` for architecture.

---

## Progress summary (as of 2026-06-06)

**Where we are:** deep into **Phase 1** (the AI-free shell), with parts of **Phase 2's
substrate** (sandbox + remote) already landed. No AI half yet (proxy/agents/MCP all
unstarted) — by design.

**Shipped & solid:**

- **Shell core** — one-session model, sidebar/panel/tabbar/statusbar WASM plugins,
  workspaces (repos) + worktrees as tabs, session detach/attach/resurrection, the
  managed `~/.superzej` zellij namespace.
- **Keybinds** — full registry, KDL splice, conflict detection, cheatsheet feed (`F`).
- **Config** — declarative TOML, layering, env/flag overlays, live reload, validation,
  95%-gated core.
- **Palette** — native iocraft Cmd-K, nucleo fuzzy + embedded ripgrep, file open.
- **Git/GitHub** — per-worktree diff (syntect-highlighted), full PR panel (status/
  checks/review/create/merge/approve/rerun) via `gh`, lazygit tool.
- **Files/editor/monitor** — yazi bottom drawer, fuzzy finder + ripgrep search,
  `$EDITOR` tool, embedded system/GPU monitors, tabbar stats widget.
- **Activity dots** — host-side `none→active→quiet→acked` state machine (`activity`).
- **Sandbox + remote (Phase 2 substrate)** — per-worktree podman/docker/bwrap/none
  backends, bind-mount-at-real-path, remote worktrees over ssh/mosh.

**Notable Phase-1 gaps (candidate next work):**

- **E. Pinned programs / tiles** — the configurable pin system is essentially
  unstarted; the Phase-1 milestone is literally a "worktree/**pin** manager".
- **AI. Notification bus** — only activity dots (425) exist; no event→action rules
  (420), desktop notifications (421), or aggregated bus (430).
- **B.** multi-select/context-menu/badge-count tree polish (26–28).

**▶ Selected next feature: E. Pinned programs / tiles** — build the config-driven
pin system, the keystone of the "worktree/**pin** manager" Phase-1 milestone.
Scoped first slice:

- **62** — `[[pins]]` config block (name, command, cwd, `location`, `scope`).
- **57 / 59** — render a pin to a **top strip** (and reuse the existing float path).
- **60 / 61** — `scope = global | workspace` resolution.
- **63 / 66** — eager-vs-lazy start; persist running pins across workspace switches.
- **70 / 74** — pin label + running/stopped glyph; launch-or-focus toggle (`Alt-1..9`).

Defer to a follow-up slice: 64 (restart-on-exit), 65 (singleton/multi), 67 (promote
running pane), 68 (unpin at runtime), 69 (strip sizing), 71 (env injection),
72 (health/auto-restart). Reuse: the `[[tools]]`/`[[agents]]` config + launch path
(73), the float/embed plumbing from tools/monitors/drawer, and the tabbar/statusbar
chrome for the strip.

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
                + cost/limits      │
                + brokerage        ▼
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
  editor **AG (405–406, 408–409)**, palette **M (161–166)**, theming
  **N (171–176, 181)**, notification bus + basics **AI (419–421, 425, 430)**, monitor
  **AH (411, 413, 415)**, basic remote **J (121–123, 130, 132–133)**, defaults + install
  **AO (493–494)**
- **Milestone:** a genuinely useful zellij worktree/pin manager. Ship, dogfood,
  get users — de-risks the whole project before any AI complexity.

### Phase 2 — Sandbox + inference plumbing · P1 · the AI substrate

Buildable in parallel with Phase 1's tail; **validatable standalone** (point an
existing Claude Code/Codex at the proxy; run dev containers by hand) before any
orchestrator.

- Containers: **AB (349–362)**, networking **AC (363–369)**, observability
  **AD (373–376, 381–382)**. Nix devshell (359) optional here.
- **The proxy: U (271–288)** — the keystone. Then cost/limits **V (289–300)** and the
  brokerage subset of **AJ (431, 433, 434, 437, 438, 441)** (virtual keys = 287/433).
  Token reduction **W (301–308)** rides along.
- **Milestone:** sandboxed envs + a metered, failover-capable proxy usable with
  off-the-shelf agents. AI-free users gain sandboxes too.

### Phase 3 — Agent layer · P1 · the headline

Depends on Phase 1 (shell) + Phase 2 (proxy + containers).

- Orchestration core **Q (211–224)** (defer 225–228)
- **ACP client first: R (229–235)**, then native adapters **(236–239, 242)** as enhancement
- Observability **S (243–258)** (tokens/cost 249–250 light up because the proxy exists)
- Review/merge basics **T (259–263, 267–268)**
- **Milestone:** spawn, monitor, review, and merge agents across worktrees, metered
  and sandboxed.

### Phase 4 — Differentiation · P2

The "magical" layer; mostly composition of what's built.

- **Semantic git X (309–317)** → upgrades review/merge (264, 265, 266, 270). sem alone
  (309–313, 317) enriches Phase-1 git, so pull earlier opportunistically.
- **GitHub Z (331–340)** + **Linear AA (341–348)**
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

- [~] 1. Coordinator daemon — host-side Rust service owning all state
- [x] 2. zellij substrate — multiplexing/rendering layer underneath
- [x] 3. Thin zellij WASM plugins — dashboard, tree, git, monitor panes
- [x] 4. Daemon↔plugin IPC — local socket + pipe
- [x] 5. Single-binary distribution
- [~] 6. One core, many front doors — TUI/API/MCP share logic
- [~] 7. Headless daemon — UI attaches/detaches
- [ ] 8. Daemon supervision — crash recovery
- [ ] 9. Internal event bus — normalized events _(only pipes + the watch daemon; no first-class bus)_
- [x] 10. Embedded state store — sqlite
- [x] 11. Config hot-reload — without dropping sessions
- [x] 12. Structured daemon logging

### B. Workspace bar / tree

- [x] 13. Left sidebar workspace tree
- [x] 14. Workspaces = repos (top level)
- [x] 15. Worktrees nested under workspaces
- [x] 16. Collapse/expand workspaces
- [x] 17. Persist collapse state
- [x] 18. Status glyphs — branch, dirty, ahead/behind
- [~] 19. Running program/agent indicator per row
- [x] 20. Contextual auto status dots (zellaude-style) _(host-side state machine; `activity`)_
- [~] 21. Fuzzy filter the tree
- [ ] 22. Manual reorder / pin-to-top
- [~] 23. Sort modes — recent/name/activity
- [x] 24. Quick-jump to numbered item
- [x] 25. Adjustable/collapsible bar width
- [ ] 26. Multi-select for bulk actions
- [ ] 27. Row context menu
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
- [ ] 37. Non-git directory as workspace
- [ ] 38. Workspace-level env vars
- [ ] 39. Workspace icon/color label
- [x] 40. Recent/favorite workspaces

### D. Worktrees

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
- [ ] 54. Worktree templates — layout+programs+container preset
- [~] 55. Worktree↔PR mapping
- [~] 56. Bulk worktree cleanup

### E. Pinned programs / tiles ◀ **NEXT FEATURE TARGET** (see ▶ below)

- [ ] 57. Pin to top strip
- [ ] 58. Add anywhere (into active layout)
- [ ] 59. Floating/scratch pin _(tools/drawer/monitors are floats, but not a pin system)_
- [ ] 60. Global pins (everywhere)
- [ ] 61. Workspace-scoped pins
- [ ] 62. Pin definition in config — cmd/args/cwd/location/scope _(adjacent: `[[tools]]`/`[[agents]]` config)_
- [ ] 63. Eager vs lazy start
- [ ] 64. Restart-on-exit policy
- [ ] 65. Singleton vs multi-instance
- [ ] 66. Persist daemons across workspace switches
- [ ] 67. Promote running pane to pinned
- [ ] 68. Unpin at runtime
- [ ] 69. Top-strip sizing/ratio
- [ ] 70. Program labels + status glyph
- [ ] 71. Per-program env injection
- [ ] 72. Health monitoring/auto-restart
- [~] 73. Program adapter — launch/notify/restart spec _(launch spec exists via `[[tools]]`/`[[agents]]`)_
- [ ] 74. Quick-toggle visibility

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

### G. Panes & layouts

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

### H. Profiles

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
- [ ] 153. Notification badges
- [ ] 154. Now-playing / arbitrary program widget
- [ ] 155. Next calendar event widget
- [~] 156. Remote/network status widget
- [ ] 157. Proxy upstream health widget
- [~] 158. CI/PR check status widget
- [ ] 159. Composable widget config
- [ ] 160. Click-through to detail views

### M. Command palette / launcher

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
- [ ] 226. Scheduled/cron tasks _(deferred)_
- [ ] 227. Task dependencies (run-after) _(deferred)_
- [ ] 228. Task priority _(deferred)_

### R. Agent integration protocols

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
- [~] _(current: `pick_agent` launches claude/aider/shell as the worktree process)_

### S. Agent observability

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

### T. Agent review & merge

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

### U. LLM proxy

- [ ] 271. Dual-protocol proxy — Anthropic + OpenAI
- [ ] 272. Hook up any provider
- [ ] 273. Aggregate models — standard/fast/free
- [ ] 274. Ordered sequential failover
- [ ] 275. Limit-exhaustion detection
- [ ] 276. Reset-window / Retry-After tracking
- [ ] 277. Automatic failback (half-open probing)
- [ ] 278. Per-upstream circuit breaker
- [ ] 279. Retries with backoff
- [ ] 280. Key/upstream load balancing
- [ ] 281. Model/tier aliasing
- [ ] 282. Auto-downgrade under pressure
- [ ] 283. Local model upstreams (Ollama/vLLM)
- [ ] 284. Prompt-cache preservation (native Anthropic path)
- [ ] 285. Streaming passthrough (no buffering)
- [ ] 286. Tool-call field preservation
- [ ] 287. Per-agent virtual keys
- [ ] 288. Proxy managed as daemon/pinned program

### V. Cost / limit / budget

- [ ] 289. Per-request cost logging
- [ ] 290. Spend attribution — agent/worktree/workspace
- [ ] 291. Spend-mode vs subscription-mode accounting
- [ ] 292. Budget caps ($/tokens) per scope
- [ ] 293. Enforce caps (refuse/downgrade)
- [ ] 294. RPM/TPM rate limiting
- [ ] 295. Daily/weekly/monthly ceilings
- [ ] 296. Kill-switch on breach
- [ ] 297. Cache-hit-ratio tracking
- [ ] 298. Spend history + export
- [ ] 299. Cost dashboards/charts
- [ ] 300. Quota refresh tracking/forecast

### W. Token reduction (rtk)

- [ ] 301. Built-in rtk output compression
- [ ] 302. Auto-hook rtk into agent bash calls
- [ ] 303. rtk telemetry off by default
- [ ] 304. Per-command bypass
- [ ] 305. Route file reads through rtk
- [ ] 306. Tokens-saved tracking
- [ ] 307. Configurable aggressiveness
- [ ] 308. Custom rtk filters per project

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

- [x] 319. Per-worktree status/diff
- [~] 320. Stage/commit from TUI
- [~] 321. Merge/rebase from TUI
- [ ] 322. Conflict resolution UI
- [~] 323. Branch management
- [~] 324. Log/graph view
- [~] 325. Blame view
- [~] 326. Stash management
- [x] 327. lazygit pin (fallback)
- [ ] 328. Commit signing
- [~] 329. Hooks-aware (pre-commit)
- [ ] 330. Cherry-pick/revert

### Z. GitHub

- [x] 331. PR tracking
- [x] 332. CI checks status
- [~] 333. PR review comments
- [~] 334. Issues
- [x] 335. Create PR from worktree
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

- [~] 419. fs-watch triggers (notify) _(notify wired, but only drives panel diff refresh)_
- [ ] 420. Rules engine — event→action
- [ ] 421. Desktop notifications (notify-rust) _(no notify-rust dep yet)_
- [ ] 422. Push to phone (ntfy)
- [ ] 423. Push to phone (Telegram)
- [ ] 424. Per-event opt-in
- [x] 425. Contextual tree dots _(activity-dot state machine)_
- [ ] 426. Do-not-disturb / quiet hours
- [ ] 427. Per-profile routing
- [ ] 428. Notification history/center
- [ ] 429. Sound/bell config
- [~] 430. Aggregated bus across all sources

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
- [ ] 504. Scriptable automations / macros — event-bus triggers → action-API actions
- [ ] 508. Whole-workspace snapshot (env+state) — Nix devshell + container checkpoint + session snapshot
- [ ] 509. Offline mode (local models only) — offline aggregate of local upstreams; graceful degradation

### AI-free mode (audience-widener)

- [~] 511. AI-free mode — run as a pure terminal workspace/worktree manager, no agents/proxy/LLM
- [~] 512. All features usable manually — git, worktrees, containers, pins, comms tiles, monitoring with zero AI
- [ ] 513. Compile-out AI components — feature flag for a lean binary without proxy/agent/MCP layers
- [~] 514. Graceful degradation — AI panels, dots, cost widgets simply absent; nothing else breaks
- [x] 515. No-AI privacy posture — zero outbound model traffic, smaller attack surface, fully local
