# THE-69 — Aura ideas audit

Status: architecture decision and coder specification. This is an audit of the
Aura README summary supplied in the issue, not a proposal to import Aura or to
build a desktop client.

## Decision

Aura's useful ideas split into three groups:

1. Several are already present in thegn's terminal-native architecture: one
   worktree per unit, declarative pipeline stages, chunk scope/dependency gating,
   merge-queue test gating before CAS, usage surfaces, and a shared notification
   model.
2. A small missing edge projection is cheap and standards-compliant: surface a
   notification when an existing `[model_proxy.budget]` rolling cap is reached.
   This is the only adopt-now implementation chunk below.
3. Semantic AST history, a desktop application, an autonomous Crew scheduler,
   commit-intent inference, and session context compression are either a poor fit
   for a terminal shell or require a larger provider/policy seam. They are
   follow-ups (or rejects), not coder chunks for this issue.

The budget chunk deliberately does **not** add `[usage].budget`. The authoritative
cap already exists at `[model_proxy.budget]` and is enforced at the model-proxy
edge. Provider subscription quotas are not a uniform spend signal, and the
current usage model is explicit about attribution limits; duplicating that state
under `[usage]` would create two cap authorities.

## Standards and ratchets applied

- `thegn-core` remains substrate-free and pure for policy decisions. That is the
  crate boundary and unit-test rule in `docs/ARCHITECTURE.md:9-36`; the existing
  chunk gate demonstrates the intended shape in
  `crates/thegn-core/src/pipeline_chunk.rs:1-21`.
- No scheduler, daemon, renderer, or provider SDK is added. The event loop keeps
  the 0%-idle contract and off-loop I/O rule from
  `docs/ARCHITECTURE.md:54-84`. Budget rows are read by the existing usage worker
  in `actions.rs`, and notification routing uses the existing chokepoint.
- Existing notification routing remains the one attention surface. `UsageLimit`
  is already an alert-priority kind in
  `crates/thegn-core/src/notification.rs:90-110,227-242`, and emit-once routing
  already persists before transient channels in
  `crates/thegn-host/src/notify.rs:359-385,423-436`.
- No capability is introduced, so the one-catalog rule in
  `docs/ARCHITECTURE.md:151-197` and the control-schema snapshot do not need a
  new entry. No config key is introduced, so the env-overlay ratchet and example
  key table remain unchanged. The help page is updated only to describe the
  newly true behavior of an existing key.
- The chunk must add a module rather than grow `run.rs` or another god file;
  this follows `CLAUDE.md`'s extraction/ratchet rule and
  `docs/ARCHITECTURE.md:199-228`. The three `spawn_usage` call sites in `run.rs`
  remain thin argument plumbing only.
- Git remains the source of truth. The budget table is an accounting cache and
  the notification row is a durable cache projection; neither becomes pipeline
  authority. This is consistent with `docs/ARCHITECTURE.md:230-240`.

## OpenSpec draft verification

There is no `openspec/changes/THE-69` or Aura-named change on this branch; the
issue body contains only the upstream URL. I therefore treated the nearest
pipeline/agent drafts as the required draft set and re-checked their claims
against current code:

- `openspec/changes/add-agent-orchestration-surface/` is the earlier generic
  agent/control-plane draft. Its “no native fleet scheduler; agent-free shell is
  additive” boundary is still the right boundary. The current pipeline config is
  explicitly declarative and validation/display-only at
  `config/config.toml.example:1532-1571`.
- `openspec/changes/add-agent-task-engine/` is the earlier generic task-session
  and merge-queue draft. Its provider-neutral command seam and existing merge
  queue remain satisfied by the current agent/task and integrate surfaces; it
  does not justify Aura-specific vendors or a second scheduler.
- `openspec/changes/improve-agent-pipeline-v2/` predates the current THE-86
  chunk gate. The draft's requested `files`, `overlaps`, and `after` frontmatter
  behavior is already implemented and verified in
  `crates/thegn-core/src/pipeline_chunk.rs:26-41,60-77` and
  `crates/thegn-host/src/cmd/dispatch.rs:277-409`. Its “verify committed
  artifacts before done” direction is likewise already represented by the
  pipeline skill's verification contract at
  `extensions/skills/pipeline/SKILL.md:250-271`.

No draft requirement that is already landed is repeated as a chunk.

## Comparison matrix

| Aura idea                                                                       | thegn equivalent today (file:line)                                                                                                                                                                                                                             | Gap                                                                                                | Verdict                                                                                                                                                            |
| ------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Semantic version control: AST Merkle graph, function rewind, node-level history | Read-only structural diff is an optional edge projection, not core state: `crates/thegn-host/src/diff_view.rs:1-13,26-39`; structural diff help: `docs/help/git-and-diffs.md:43-64`                                                                            | No durable semantic IDs, AST graph, function rewind, or semantic source of truth                   | **reject with reason** — a persistent AST graph and function-level mutation model do not fit the terminal shell's git source of truth or current bounded diff seam |
| Semantic review: logic-node PR diff and downstream impact                       | The PR/diff surfaces remain file/line-oriented; the internal diff model is `PrDiff` plus optional structural rendering: `crates/thegn-host/src/diff_view.rs:30-39`                                                                                             | No dependency-impact graph or semantic provenance timeline                                         | **file-follow-up** for a bounded, read-only provenance view; the AST graph portion remains rejected                                                                |
| Intent gatekeeper at commit                                                     | Dispatch records currently carry issue/worktree/parent/artifact/chunk fields, not an intent policy: `crates/thegn-core/src/issue.rs:223-266`                                                                                                                   | No provider-neutral declared-intent syntax or commit-time policy                                   | **file-follow-up** — policy and Git-hook seams need design, trust rules, and explicit false-positive behavior                                                      |
| Mission Control: multiple agents in one window                                  | Terminal panes/tabs and generic configured launch commands are the native surface; the supervisor is a coordinator, not a fleet driver: `extensions/skills/supervise/SKILL.md:6-22`; pipeline agent fields are generic: `config/config.toml.example:1548-1571` | No transcript compression or vendor-specific session federation                                    | **adopt-now** — the terminal-native equivalent already exists                                                                                                      |
| Session handoff and context compression                                         | Durable pipeline resume tracks roster state and artifacts: `extensions/skills/pipeline/SKILL.md:138-160,208-249`                                                                                                                                               | No provider-neutral transcript/context compaction contract                                         | **file-follow-up** — add an optional artifact-based handoff seam, never hard-code Claude/Gemini/Cursor/Kimi                                                        |
| Crew dependency-aware in-repo task graph                                        | Pipeline stages/roster are declarative: `config/config.toml.example:1532-1608`; chunk `after` and file overlap gates are live: `crates/thegn-host/src/cmd/dispatch.rs:277-409`                                                                                 | No general persisted DAG or ready-node scheduler; Lead judgment intentionally remains in the skill | **file-follow-up** — a richer DAG can reuse the current pure gate without becoming an autonomous scheduler                                                         |
| Crew's cheap chunk-dependency field                                             | `after:` is parsed and enforced against same-issue, same-worktree dispatch rows: `crates/thegn-core/src/pipeline_chunk.rs:26-41,60-77`; `crates/thegn-host/src/cmd/dispatch.rs:322-409`                                                                        | No gap for the requested small dependency seam                                                     | **adopt-now** — already satisfied by THE-86; no chunk                                                                                                              |
| Automatic parallel worktrees                                                    | One worktree per unit and fan-out/reuse are pipeline rules: `extensions/skills/pipeline/SKILL.md:162-175`; worktree registration has rollback on DB-registration failure: `crates/thegn-host/src/cmd/wt.rs:246-290`                                            | No Aura-style automatic dispatcher                                                                 | **adopt-now** — current Lead/dispatch flow already supplies the seam                                                                                               |
| Rollback after a test failure                                                   | Fold is pure and defers conflicts without partial replay: `crates/thegn-core/src/fold.rs:13-21`; the host tests the folded tip in a stable/throwaway gate worktree before CAS: `crates/thegn-host/src/integrate.rs:650-727,796-820,974-979`                    | No post-mutation rollback transaction is needed                                                    | **adopt-now** — already satisfied in the safer pre-CAS shape; do not add destructive rollback after advancing the target                                           |
| Usage tracking surfaces                                                         | `[usage]` has status/overlay/panel and off-loop provider gathering: `crates/thegn-core/src/config.rs:1528-1632`; usage worker and proxy spend rollup are off-loop: `crates/thegn-host/src/actions.rs:353-444`                                                  | No gap in the existing status/overlay/panel surfaces                                               | **adopt-now** — already satisfied                                                                                                                                  |
| Per-session cost and burn rate                                                  | The usage model separates provider windows from host-wide transcript totals: `docs/help/ai-usage.md:127-152`; model-proxy audit rows retain scoped token/cost metadata: `crates/thegn-core/src/store/model_proxy.rs:11-47`                                     | Session identity and a uniform rate basis are not yet honest across providers                      | **file-follow-up** — define attribution before adding a new gauge or cap                                                                                           |
| Per-session/project budget caps with notification                               | Model-proxy budgets already persist rolling per-scope spend and kill state: `crates/thegn-core/src/store/model_proxy.rs:49-63,78-94`; enforcement is at the proxy edge: `crates/thegn-proxy/src/budget.rs:147-188`                                             | Cap breaches are not yet projected through the usage notification path                             | **adopt-now** — one small chunk projects existing caps as `UsageLimit`; no new cap config                                                                          |
| Desktop Aura application                                                        | thegn is an embedded terminal compositor with the terminal/UI boundary fixed in the architecture: `docs/ARCHITECTURE.md:9-21`                                                                                                                                  | A second desktop client would duplicate state, rendering, input, and capability projection         | **reject with reason** — outside the product and substrate boundary                                                                                                |

## Follow-ups to file

**Intent-at-commit policy.** File a follow-up for a pure core `IntentPolicy` that
compares declared structured intent facts with a host-provided changed-path and
diff summary. The host may install an opt-in Git commit-msg/pre-commit adapter,
but the core must only return explainable facts; no model call, vendor SDK, or
silent commit rejection belongs in core. The default should report a
notification/diagnostic and allow the user to choose enforcement, with tests for
missing declarations, renames, generated files, and false positives.

**Provider-neutral handoff.** File a follow-up for a committed handoff artifact
under the existing per-issue pipeline namespace. A pure core envelope should
carry task, files, tests, status, and bounded context references; a host/provider
adapter may optionally compress text off-loop. Resume must work with no provider
or compressor, and the Lead remains responsible for deciding when to hand off.

**Richer dependency graph.** File a follow-up for an explicit, git-visible DAG
artifact that the core parses and topologically classifies, reusing the existing
`after`/scope verdict types. The host can display ready/blocked nodes, but
dispatch remains an explicit Lead action; no daemon should auto-launch every
ready node, because that would violate the current declarative pipeline contract
and make concurrency/timeout policy implicit.

**Session budget and burn rate.** File a follow-up only after defining an honest
session identity and spend source. The existing model-proxy budget should remain
the enforcement authority; a future host projection can add session labels and a
pure rate calculation, then feed the existing statusbar and `UsageLimit` route.
Provider subscription windows must remain distinct from metered proxy spend, and
unknown/unreadable data must degrade to “unavailable,” never zero.

**Semantic provenance.** Do not file a semantic AST-engine implementation. If
users later need provenance, file a narrow read-only provider seam that can show
external tool output alongside ordinary git diffs, with no AST graph in
`thegn-core`, no function-level rewrite, and no new source of truth.

## Adopt-now chunk inventory

Only `chunk-1.md` is authorized. It is deliberately limited to notification of
already-enforced model-proxy caps; all other rows above are existing behavior,
rejects, or follow-ups.
