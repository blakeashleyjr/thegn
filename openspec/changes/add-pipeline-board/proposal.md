# Add the pipeline board — the agent roster as a stage-grouped surface, and the adopt drain that makes stage agents watchable

> **SUPERSEDES `openspec/changes/add-fleet-view`.** That change is archived in
> place rather than implemented: its data source (the excised LLM proxy) is back
> as `resurrect-model-proxy`, but its _framing_ — a "fleet" of agents with a
> `thegn fleet` verb — was overtaken by the agent-orchestration work, which
> explicitly reserved that noun. What survives here is the part that was right:
> **the render invariant** (a live agent surface is a bounded diff, never a Full
> chrome recompose) and the choice of a roster-backed, off-loop-hydrated surface.
> What is dropped: the `fleet` noun, the `thegn fleet` CLI verb, and the
> per-agent token/context/cost metrics (deferred to phase 2, where they are a
> groupby over `model_proxy_requests` — data that has already landed).

## Why

Three things are true today and they do not add up to a watchable pipeline:

1. **The dispatch roster has no UI at all.** `agent_dispatches` is a durable
   ledger — and since `add-pipeline-roster-stages` it carries `stage`,
   `parent_id`, `session_id` and `artifact_path` — but the only way to read it
   is `thegn dispatch list`. A supervising agent fanning work across an
   Architect and three coders is invisible to the human supervising _it_.
2. **The sidebar shows worktrees, not stages.** With several stages sharing one
   worktree (the case the `session_id` attribution fix exists for), the sidebar
   row says a worktree is busy but never says _at what_.
3. **`sessions.open --adopt` is inert.** The flag files an `adopt_session`
   intent, the payload type is documented, the daemon writes the row — and
   **nothing reads it**. The only occurrences in the tree are the producer, the
   type, and prose. So every stage agent launched from outside the UI stays
   headless, and the `intents` table grows a row per launch that no consumer
   will ever claim. The user's stated decision — "each stage agent appears as a
   live pane in its worktree's tab" — is not deliverable until something drains
   that mailbox.

## What Changes

1. **The adopt drain (the load-bearing half).** The compositor claims
   `adopt_session` intents on its hydration pass and grafts each named daemon
   session into a real pane in that session's worktree tab group, honouring
   `focus: false`. It reuses the one existing adoption door — the warm-reattach
   branch's `spawn_daemon_backed(..., attach = Some(session))` — so an adopted
   pane is an ordinary daemon pane in every respect (relay, reconnect ladder,
   `pane_sessions` capture, restart survival). Claim-and-delete is drain-all, so
   the unbounded accumulation stops; rows older than a five-minute cutoff are
   claimed and **dropped**, so a mailbox filled while no UI ran does not erupt
   into panes for dead sessions at the next launch.
2. **A `MonitorTab::Pipeline` board**, cloned from the Containers precedent: an
   overlay tab, not a panel `Section`. Rows are the roster grouped by stage
   (config order first, unknown stages after, `NULL`-stage rows trailing as
   `unstaged`), chunk rows indented under the parent they were fanned out of,
   each row carrying the status glyph / agent / worktree / issue / age. Hidden
   until a roster row exists or a pipeline is configured.
3. **Row activation**: `Enter` on a board row escalates a
   `MonitorAction::Pipeline(PipelineJump)` and lands the dispatch's worktree
   through the same `activate_row_target` door a sidebar `Enter` takes.
   Pane-level (session) focus is phase 2; the request already carries the
   session id for it.
4. **Off-loop hydration**: a new `RefreshKind::Dispatches` payload variant,
   sampled off the loop while the board is the live view (the Containers
   liveness-gate pattern), seeded once so the hidden tab can discover it has
   rows, and kicked on change from the pane-exit path. No new timer, no new
   thread, no new wake source.
5. **A sidebar stage tag**: `SidebarRow.pipeline_stage`, denormalized from
   evidence exactly like `mq_status`, painted beside the activity dot via the
   `row_is_blocked` precedent. A `waiting_human` roster row feeds the
   **existing** blocked evidence (`AttentionInputs::stage_blocked_since` scoring
   through the existing `AgentNeedsInput` reason), so the red-vs-amber dot and
   the needs-you ring cover pipeline stages with **no new `ActivityState`, no
   new `AttentionReason`, and no new `NotificationKind`**.

## Impact

- **Q 212** (Task→worktree→agent→review→merge pipeline) — this is its rendering
  half; the roster/config halves are changes 1 and 2 of the same plan.
- **S 243/244/245** (contextual status dots, abtop-style fleet view, "working
  now?") — the board is the re-scoped, roster-backed realization of 244; the
  stage tag is 243's evidence-driven form.
- Extends the `agent` capability spec. **No DB schema change** — every column
  the board reads landed with `add-pipeline-roster-stages` (v56).
- **No new action, no new keybind, no new help context.** The board is reached
  by the existing monitor-open action plus the existing tab-cycle/digit keys, so
  the action checklist and the `panel:*` help-context ratchet do not apply (see
  design.md).

## Doctrine

`[[pipeline.stages]]` encodes **structure, not judgment**. This change only
_groups and labels_ by stage and _shows_ what the roster says. No code path here
advances a stage, enforces a concurrency limit, or times a row out: the board is
read-only over the roster, and the roster gains columns, never transitions.
Stage transitions remain the supervising agent's, written through
`dispatches.put`.
