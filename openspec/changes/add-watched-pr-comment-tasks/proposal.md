# Watched PRs: act on unresolved comments, pick the agent per entry

Linear: THE-22

## Why

The PR queue (`add-pr-queue`, implemented) watches a pull request and wakes an
agent for three blocker classes — but its review trigger is the forge's
aggregate `review_decision == CHANGES_REQUESTED`. Most day-to-day feedback
never sets that: a reviewer leaves two inline threads and approves, or
comments without submitting a formal request-changes review. Those PRs sit at
`awaiting_review`/`none` forever while the driver _already fetches their
threads_ (`pr_driver.rs` counts unresolved threads for the blocker hint) and
the `PrReview` task _already formats unresolved threads into its prompt_. The
trigger is the only missing piece — the queue can see the feedback and knows
how to hand it off, it just never fires.

Two more gaps surfaced by the seed issue (orca#7465):

- **One global agent.** `[pr_queue] agent` is repo-wide; the issue's actual
  ask is picking the agent per PR ("some PRs may need to be handled by
  different model capabilities").
- **No re-arm on new feedback.** The attempt budget refills on a new head
  OID, but a _new comment_ on an already-tried PR is equally new information
  and today leaves a `needs_human` row stuck.

## What Changes

- **Unresolved threads become a classified blocker.** New
  `Blocker::UnresolvedComments(n)` (wire word `unresolved_comments`),
  reported when a queued PR has unresolved review threads and no stronger
  blocker; it maps to the existing `PrWatchKind::Review` (whose config
  aliases already read `"review" | "comments"`) and dispatches the existing
  `PrReview` task kind — same prompt, broader trigger. A new
  `[pr_queue] review_trigger = "changes_requested" | "any_unresolved"`
  (default `changes_requested`, today's behavior) gates whether the blocker
  is _actionable_; it is always _displayed_ ("2 unresolved comments") so the
  row tells the truth either way.
- **Re-arm on new comments.** Each entry records a fingerprint of its
  unresolved thread ids. When the fingerprint changes through an event thegn
  did not cause (a new thread appears), the agent attempt budget refills —
  the exact analogue of the existing head-OID refill, and with the same
  anti-loop property: the agent's own replies do not change the unresolved
  set, so it cannot refill its own budget.
- **Per-entry agent override.** `pr queue add --agent <name>` (an
  `[[agents]]`/`[[tools]]` entry) and `--agent-command <template>` store an
  override on the row, used at dispatch instead of `[pr_queue] agent`; a
  panel row action sets/clears it; `pr queue list --json` reports it.
- **A new-feedback notification.** A watched entry whose unresolved
  fingerprint changes raises a notification — including entries the agent
  will never touch (`own_prs_only`, foreign author), which is what makes
  _watching_ a teammate's PR useful.

## Impact

- Roadmap: extends **Z 759** (PR queue, team mode); advances **Z 338**
  (PR event notifications) and **AT 645/646** (threaded review comments,
  two-way sync) on the acting side; complements **T 262** via the shared
  single-thread prompt formatter from `add-pr-comments-in-diff`.
- Specs: `pr-queue` — ADDED requirements (layers on the in-flight
  `add-pr-queue` deltas; this change lands after it syncs). `state-db` —
  ADDED columns on `pr_queue` (agent override, thread fingerprint) ⇒
  **`user_version` bump**, additive migration.
- Code: `thegn_core::pr_queue` (new blocker arm + refill rule — pure,
  table-tested under the 95% gate), `config_pr_queue.rs` (`review_trigger`),
  `pr_driver.rs` (thread fetch on the poll path, fingerprint, override
  resolution), `cmd/pr_queue.rs` (`--agent`/`--agent-command`),
  `handlers/pr_queue.rs` + `panel/sections/pr_queue.rs` (row action, display),
  one new `NotificationKind`.
- Config: `review_trigger` documented in `config/config.toml.example`;
  CLI flags project the existing `pr queue add` catalog row (no new verb, no
  new capability-catalog entry).
- Help: `docs/help/pr-queue.md` (context `panel:prq`) gains the new key,
  flag, and notification prose (prose ratchet).
- In-flight overlap: **depends on `add-pr-queue`** (all of it);
  `add-pr-comments-in-diff` (THE-27) shares the thread prompt formatting;
  `add-issue-autopilot` (THE-56) composes on top of this trigger for its
  end-to-end loop. Reconciled with `add-generic-tracker-model` (no overlap —
  tracker side untouched).

## Non-goals

- **Resolving threads.** Reply-never-resolve stands.
- **Acting on PR-level (non-thread) conversation comments or body task-list
  checkboxes.** Only review threads carry a resolved bit the queue can
  classify honestly; plain comments have no completion state, so acting on
  them cannot terminate. Display-only today; revisit if a forge grows
  actionable state for them.
- **Per-thread dispatch granularity.** The queue dispatches per-PR (the
  `PrReview` prompt lists every unresolved thread); one-thread-at-a-time
  handoff is the manual flow in `add-pr-comments-in-diff`.
- **Changing any team-safety default.** `own_prs_only`,
  `pause_on_foreign_push`, force-with-lease, never-merge/approve/resolve all
  stand unmodified; the new trigger defaults to off
  (`review_trigger = "changes_requested"`).
