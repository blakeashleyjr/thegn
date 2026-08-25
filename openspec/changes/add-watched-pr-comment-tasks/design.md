# Design — comment tasks on watched PRs

## Threads join the classify inputs

`classify` today reads only `PrStatus` fields ("no extra fetching is needed to
decide"). Unresolved threads are not on `PrStatus`, so the input grows
honestly rather than by side channel: `classify(&PrStatus, unresolved:
&ThreadsFingerprint, cfg)` (or an equivalent `PrQueueSignals` struct) where
the driver supplies the thread summary it already fetches per poll for the
blocker hint. The ordering slot is deliberate:

```
Closed > Draft > Ci > Conflict > ChangesRequested > UnresolvedComments
      > ChecksPending > AwaitingReview > None
```

`ChangesRequested` subsumes `UnresolvedComments` (a request-changes review
almost always carries threads; the formal decision is the stronger, clearer
signal). `UnresolvedComments` outranks `ChecksPending`/`AwaitingReview`
because it is _actionable_ — the same reason a red check outranks a stale
base today. `Blocker::UnresolvedComments(n)` carries the count for display;
`watch_kind()` returns `PrWatchKind::Review`; `task_kind()` returns
`TaskKind::PrReview`.

**Actionability vs display:** classification always reports the blocker (the
row must tell the truth), but `decide` treats `UnresolvedComments` as
dispatchable only when `review_trigger = "any_unresolved"`. With the default
`changes_requested`, the row shows "2 unresolved comments" and waits — no
behavior change for existing configurations. All of this is pure and
table-tested (95% core gate), like every other team-safety rule.

## Fingerprint + refill

The row stores `threads_fingerprint`: a stable hash over the sorted ids of
unresolved threads (ids, not count — one thread resolved plus one opened must
read as changed). Two consumers:

1. **Budget refill** — `attempts_reset` gains the fingerprint alongside the
   head OID: budget refills when the fingerprint changes _and_ the change
   introduces at least one thread id thegn has not seen for this row. The
   agent's own replies never alter the unresolved id set (reply ≠ resolve),
   so — like the head-OID rule recording the OID thegn produced — the agent
   cannot refill its own budget.
2. **Notification** — a fingerprint change with a new thread id raises
   `NotificationKind::PrQueueNewComments` (exact name per the existing
   pr-queue kinds' convention), for every watched row including foreign-author
   rows the agent will never touch.

Fetch cost: the driver already calls the forge's `review_threads` on the
dispatch path; the poll path adds it only for rows whose blocker resolution
could change (open, non-draft), under the existing per-row backoff. A thread
fetch failure leaves the previous fingerprint intact — never fabricate
"changed" from an error (mirrors the existing fetch-failure rule).

## Per-entry agent override

Two nullable columns on `pr_queue`: `agent` (an `[[agents]]`/`[[tools]]` name)
and `agent_command` (a full template). Dispatch resolution order becomes
row-`agent_command` > row-`agent` > `[pr_queue] agent_command` >
`[pr_queue] agent`, all through the existing `agent_task::resolve_agent` /
template validation — an invalid row template fails dispatch with the same
diagnostics as the config one, and the row goes `needs_human` with the reason
rather than silently falling back. CLI: `pr queue add --agent/--agent-command`
(also accepted by a new `pr queue set <number> --agent …` only if trivially
cheap — otherwise the panel action suffices; decide at implementation).
Panel: a row action prompting from the configured `[[agents]]` list, clear
with an empty pick.

## Event loop, rendering, schema, help

- **Wake path:** unchanged — the existing `RefreshKind::PrQueue` ticker slot
  and push kick; the thread fetch rides the same off-loop poll pass and
  pulses the waker once. Disabled ⇒ no polling (unchanged).
- **Damage:** panel-section row changes set the master `dirty` ⇒ `Full`
  frame, exactly as today.
- **SQLite:** three additive columns (`agent`, `agent_command`,
  `threads_fingerprint`) on `pr_queue` ⇒ **`user_version` bump** with an
  additive migration (NULL for existing rows).
- **Help:** `panel:prq` → `docs/help/pr-queue.md` documents the override
  action, `review_trigger`, and the new notification (prose ratchet). No new
  action id is strictly required if the override rides the existing row-action
  menu; if a new id is minted it must be claimed by the page (help ratchet).

## Security

- **Blast radius of the broader trigger.** `any_unresolved` means a single
  reviewer comment can start an agent that ends in a push. Bounds, all
  pre-existing and unmodified: the feature is off by default
  (`[pr_queue] enabled = false`), dispatch requires the `review` watch kind,
  `own_prs_only` blocks writes to foreign PRs, `pause_on_foreign_push` stops
  races, pushes are `--force-with-lease` only, the attempt budget caps
  loops, and the agent never merges/approves/resolves. The new default
  (`changes_requested`) adds zero new autonomous behavior until a user opts
  up.
- **Prompt injection.** Thread bodies are untrusted remote text that becomes
  agent instructions — true of the existing `PrReview` path already; the
  broader trigger widens _who_ can put text there (anyone who can comment on
  the PR, which on public repos is everyone). Mitigations: the rules block in
  every PR prompt, the agent's lack of forge credentials (writes go through
  thegn's seam under thegn's policy — the agent can only edit and push the
  branch), the sandbox the pane/job runs in, and `own_prs_only` +
  `review_trigger` defaults. State plainly in docs: enabling
  `any_unresolved` on a public repo lets any commenter feed the agent text.
- **Credential handling:** none new — the forge seam and `gh` auth are
  untouched; row `agent_command` is config-equivalent user input, validated
  by the same template rules (no raw secrets; SecretRef conventions apply to
  agent config as today).
- **Notification text** renders comment excerpts through the existing
  notification pipeline (chrome-composed, no PTY) — no sanitization concern
  beyond the usual width caps.

## Alternatives considered

- **A new `TaskKind::PrComments`** — rejected: the `PrReview` prompt already
  says "unresolved review threads"; a second kind duplicates prompts, config,
  validation, and the pinned-count tests for identical work.
- **Count-based fingerprint** — rejected: resolve-one-open-one reads as
  unchanged; id-set hashing is as cheap and correct.
- **Auto-acting on PR-level comments via @mention detection** — deferred
  (non-goal): no resolved bit means no termination condition; an agent that
  replies to every comment forever is the failure mode.
- **Per-blocker-kind agent override** (different agent for CI vs review on
  one row) — rejected as config surface bloat; the issue asks per-PR, not
  per-kind.

## Open questions

- Should `review_trigger = "any_unresolved"` also require the PR to be
  otherwise green (no red CI) before dispatching a review task, to avoid the
  agent juggling two blockers? (Ordering already prefers CI first; leaning
  no extra rule.)
- Whether `pr queue set` (post-hoc override without re-adding) earns its CLI
  surface or the panel action suffices.
