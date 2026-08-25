# Design — issue autopilot

## Shape: a pickup loop and a bridge, not a new engine

Autopilot owns exactly two new behaviors — _when to start_ (pickup/claim) and
_how to hand off_ (push + create-PR + enqueue). Everything between is existing
machinery invoked as-is: worktree creation (`wt::add_checked` + the
issue-branch naming path shared with `add-issue-driven-worktrees`), headless
dispatch (`agent_task` + `agent_run`: quoting contract, watchdog, git-env
scrub, sandbox slice via `wrap_background_argv`, Windows stub), PR creation
(`forge::create_pr`), and shepherding (the PR queue, wholesale). If the PR
queue is disabled, autopilot stops at "PR opened" and says so — it never
reimplements the babysitter.

## Two prompt families, one boundary

The run's life has a hard line at PR creation, and the inverse prompt
families land on either side of it:

- **Before the PR** (`TaskKind::IssueImplement`): merge-queue-family rules —
  work only in this worktree, commit on this branch, **do NOT push**, never
  merge. The agent needs no forge credential and no network write; thegn
  validates the result (clean `git status`, ahead of base) before anything
  leaves the machine.
- **After the PR**: the PR-queue family (`PrCiFailure`/`PrConflict`/
  `PrReview`) — the agent must push (`--force-with-lease` only), never
  merge/approve/resolve — unchanged from `add-pr-queue`.

thegn performs every forge write itself (push, `create_pr`, enqueue, status
sync), so "what the agent may do" is enforced structurally, not just by
prompt text.

## Pickup, claim, and the pure core

`thegn_core::autopilot` is pure policy, table-tested under the 95% gate:

- `matches(issue, cfg) -> bool` — trigger label present, assignee policy
  satisfied (`me` = the session's tracker identity; `any` documented but the
  label is still required), status in `pickup_status`.
- `claimable(matches, runs, cfg) -> Vec<IssueId>` — dedupe against existing
  runs (any non-terminal run for the issue blocks re-claim), enforce
  `max_concurrent` across live runs, deterministic order (oldest issue
  first).
- Run state machine: `claimed → working → pr_opened → shepherding →
done | needs_human | stopped`, with legal-transition tests. `retry` is only
  legal from `needs_human`/`stopped` and consumes a fresh attempt
  (`max_attempts`, default 1 — no unattended retry storms).

The host hook runs at issue-refresh completion (already off-loop), reads the
fresh issue set + `autopilot_runs`, claims, and spawns run drivers on
`sched::spawn_bg` (Background QoS); every settled step sends on a channel and
pulses the `TerminalWaker`. **No new wake source, no new poll timer** — the
`RefreshKind::Issues` ticker slot is the cadence, and the whole hook is
skipped while disabled (the 0%-idle contract is untouched).

## Claim durability

`autopilot_runs` (state-db, `user_version` bump): issue id (unique), repo
root, worktree, branch, state, attempt count, pr number (nullable),
timestamps, last error. The claim insert is the mutual-exclusion point for
one host (SQLite unique constraint); a crash mid-run leaves a row a restart
reports as `needs_human` (with the worktree preserved for a human) rather
than silently re-dispatching — resurrection, not repetition. git remains the
source of truth for the worktree/branch; the row is bookkeeping.

## Status sync

Claim ⇒ `IssuePatch { status: InProgress }` via the existing `IssueRouter`
(off-loop, failure logged + surfaced as a run note, never fatal to the run —
the tracker is not the source of truth for the work). Merge observed (the PR
queue's settled `merged` transition for a PR autopilot opened) ⇒
`IssuePatch { status: Done }` under `done_on_merge`. `needs_human` writes
nothing. When `add-generic-tracker-model` lands, these two writes route
through its transitions API (and gain provider-honest state names); the
autopilot spec deliberately names only the two generic transitions both
models can express.

## PR derivation

Title: the issue's `number: title` (template `{identifier}`/`{title}` vars
under `[autopilot]` if trivially cheap; otherwise fixed form). Body: issue
URL + body excerpt + a provider-appropriate closing reference — GitHub
`Closes #N` only when the tracker _is_ the same GitHub repo; otherwise a
plain link (Linear's magic words are its own follow-up). `open_as = "ready"`
by default: the human gate is the PR queue's `require_approval` (default on)
plus branch protection — forge-native, one gate, where reviewers already
live. `"draft"` adds a second explicit human step for cautious teams.

## Event loop, rendering, schema, help (config.yaml checklist)

- **Wake path:** issue-refresh completion hook + `spawn_bg` run drivers +
  waker pulse; no polling added to the idle loop.
- **Damage:** run-state changes touch chrome (row badges, notifications) ⇒
  master `dirty`, a `Full` frame — same as every queue transition today.
- **SQLite:** `autopilot_runs` ⇒ **`user_version` bump**, additive
  migration.
- **Help:** new `docs/help/autopilot.md`; the CLI verbs' action ids and the
  config table are claimed there (help + prose ratchets); the config
  reference page is generated — never hand-written.
- **Catalog:** `autopilot status|stop|retry` are CATALOG rows projected
  across CLI (and thereby MCP/control surfaces), gated by
  `required_scope(verb)`; the gaps list may only shrink.

## Security

**Never without a human** — the loop's hard floor, enforced structurally
(thegn holds the credentials, the agent holds a checkout):

1. **Merge or approve a PR** — the forge merges under `[pr_queue]`
   `merge_mode`/`require_approval`/branch protection; autopilot adds no
   bypass and the agent is prompted and credential-starved against both.
2. **Force-push** — the implement stage never pushes at all; the shepherd
   stage is `--force-with-lease` only (existing rule).
3. **Push to the default/protected branch** — thegn pushes exactly one new
   branch; the merge guard and queue invariants stand.
4. **Mark a draft ready** — with `open_as = "draft"`, promotion is a human
   act.
5. **Close, re-scope, or edit tracker items** beyond the two status
   transitions and the optional pickup comment — `IssuePatch` writes are
   allowlisted to those fields in the autopilot path.
6. **Act on an unmarked issue** — the trigger label + assignee policy is
   explicit consent; there is no heuristic pickup.

**Trust boundary — who can start an agent:** anyone who can set the trigger
label and assignment in the tracker can make thegn run an agent on this
machine. Document this loudly on `[autopilot] enabled`: restrict label/assign
permissions in the tracker accordingly; `assignee = "me"` (default) requires
the issue to be assigned to _this session's_ tracker identity, so a random
reporter labeling an issue is not sufficient.

**Prompt injection:** the issue body is untrusted text handed to an agent
with local execution. Mitigations: off by default; label-gated consent;
merge-family rules block + no-push implement stage (nothing exfiltrates via
git until thegn validates and pushes one branch); the agent runs inside the
worktree under the sandbox policy and resource slice (`sandbox_cpucap` /
`wrap_background_argv`); the human gates at review. Residual risk (a
malicious body steering the _code_) is exactly the PR review's job and is
stated in docs, not hidden.

**Credentials:** no new secrets and no raw tokens in config — tracker auth
stays in the existing `[issues]` provider config (SecretRef/env:/file:
conventions), forge auth stays with the forge seam (`gh` auth today). The
agent process inherits neither a tracker token nor any forge credential from
autopilot; pushes use thegn's own git credential path.

**Blast radius of the new write surfaces:** remote branch push + PR creation

- two tracker status fields + one optional comment. Each is attributable
  (commits/PRs are authored by the configured identity; runs are journaled in
  `autopilot_runs` with timestamps and errors — the audit trail for "why did a
  PR appear").

## Alternatives considered

- **The agent opens the PR itself** (Devin-style) — rejected: hands the
  agent forge credentials and makes "never merge/approve" a prompt promise
  instead of a structural fact. thegn opening the PR also keeps templating,
  draft policy, and enqueue atomic.
- **A generic workflow/rules engine** ("on tracker event do X") — rejected
  for now: one well-shaped loop with hard rails beats a programmable one for
  a first autonomy surface; a rules engine is a plugin-api candidate later.
- **Interactive pane instead of headless run** — rejected as the _default_:
  unattended pickup with an interactive pane just parks an idle agent
  awaiting input. The manual `s`/`D` keys remain the interactive path;
  autopilot is the unattended one. (A `mode = "pane"` variant could be
  additive later.)
- **Webhooks for pickup latency** — deferred; the poll cadence bounds
  latency at minutes, acceptable for v1 and free of a listening surface.

## Open questions

- Should `stop` also delete the created worktree, or always leave it for
  post-mortem (leaning: leave it; `wt` commands already remove)?
- `pickup_status` semantics per provider before `add-generic-tracker-model`
  lands (the generic enum has no `in_review`) — start with `todo` only?
- Whether the pickup comment should include the machine/host name for
  multi-machine teams (helps the cross-host race story without solving it).
