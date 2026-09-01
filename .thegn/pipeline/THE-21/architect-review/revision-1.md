# THE-21 revision 1 — make live automations truthful, durable, and race-free

## Why this revision is required

The pure matcher, config shape, catalog projections, schema snapshot, and basic
CLI surfaces are present, but the live runtime does not yet meet the architecture
contract. Several advertised triggers/selectors cannot occur in production, the
notification seam loses routing facts, concurrent processes can bypass throttle
state, and one-shot CLI producers can exit before their queued automation runs.
These are behavior and safety gaps, not polish.

Keep architect correction commit `fda66103` (repo automation warnings are now
surfaced and audit errors are bounded).

## 1. Reconcile current main before changing the migration

Current `main` advanced during review and owns schema v62 (`session_forks`) and
v63 (`pr_review_cache`) plus hardened migration/open behavior. This branch also
claims v62 for automation tables. Merge current `main`, preserve both main
migrations, and assign automation state/audit the next free schema version
(v64 if main is still v63). Adapt the automation DDL and verification to main's
current migration ladder; do not overwrite or renumber main's tables.

Re-run the catalog/control snapshots and all migration ladder/newer-schema tests
after the merge, because main also added catalog and wire rows since this
branch's merge-base.

## 2. Finish the normalized live-event contract

### Notification seam

`automation_events::submit_notification` currently turns every inbox kind into
the same `AutomationEventKind::Notification`, discards the underlying
`NotificationKind`, and recomputes its default priority. A rule therefore cannot
select `test_failed` versus `queue_needs_human`, and a notification priority
changed by `[[notifications.rules]]` is invisible to automation. Direct producer
calls to `automation_events::emit*` also bypass `notification_route::decide`, so
they still do not share one routed chokepoint.

- Carry the stable notification kind on the normalized event and expose a
  validated predicate/template variable for it.
- Submit only after the one notification route decision. A dropped notification
  must not reach automation; a recorded notification must carry the decision's
  final effective priority.
- Preserve append versus emit-once behavior and ensure each inserted row submits
  exactly one event. Use the inserted notification row id (or another actually
  unique id) for event identity; the current hash of kind/key/whole seconds can
  collide for repeated appends in one second.
- Migrate all direct producers through this real routed adapter, not merely a
  wrapper around `Db::put_notification`.

### Producer facts

Live code never emits `PrChecks` or `PrReviewRequested`; only `MergeLanded` is
submitted. Add edge-derived producers from authoritative old/new forge cache
facts. Do not infer checks or review state from message text. A checks event must
carry `pr_checks_passed`, and a review-request event must carry the corresponding
fact. Validate fixtures so `automations test` cannot claim a typed PR match when
the kind's required fact is absent.

`workspace`, `repo`, and `agent_role` are currently never populated by any live
`EventFacts`, and `branch` is populated only for one PR path. Enrich events from
the existing worktree/session/dispatch records when those facts are known.
Do not map an agent program name to a pipeline role as a guess. Add producer-level
tests proving documented selectors can match a real notification and daemon
session edge.

`worktree_idle` has no duration field. The runtime currently treats
`debounce_secs` as the idle delay even though config and docs define it as the
post-fire throttle. Add a separate bounded idle duration (minimum 60 seconds),
validate it only where appropriate, keep debounce semantics independent, and
arm deadlines only for enabled idle rules. Base/re-arm the deadline from the
authoritative activity edge, with no compositor ticker.

## 3. Make admission and audit atomic under concurrency

`run` permits multiple `process_event` tasks to load the same SQLite state,
evaluate independently, and overwrite each other's transition. Two concurrent
events (or the UI and daemon processes) can therefore both pass debounce,
once-per-key, and hourly limits. The `max_action_per_hour` ledger is also stored
inside each rule, making the action-wide limit redundant with the per-rule limit
instead of bounding the same catalog action across rules.

- Serialize/transactionally arbitrate evaluation plus state transition before
  launching actions. The solution must be safe across processes, not only under
  one Tokio semaphore. Preserve action concurrency after admission.
- Persist the accepted state transition and its pre-dispatch audit row in one
  transaction, so state is never consumed without a run record and a run is
  never launched without its throttle state.
- Give action-rate state action-wide semantics and test two rules targeting the
  same capability.
- Keep persisted rate windows and once-per-key state explicitly bounded; the
  current `once_keys` set grows forever.
- A bounded-channel overflow must produce a durable `dropped` audit outcome
  without blocking the producer/render path. Today it only logs a warning.
- Keep every SQLite open/read/write and control-address discovery on a blocking
  worker. The failure-notification write and `automation_executor::client()`
  currently perform synchronous DB work from async runtime threads.

Add deterministic runtime tests that hold two admissions at the race boundary
and prove once/debounce/rule-rate/action-rate cannot double-fire, plus an
overflow test that observes the durable dropped row.

## 4. Fix process ownership, dry-run isolation, and status honesty

`run_subcommand` installs a detached automation thread for every CLI verb. A
one-shot producer such as `thegn notify push` can enqueue an event and then exit,
terminating the worker before evaluation/action/audit completes. Either forward
events to one durable daemon-owned runtime or provide an explicit bounded drain
contract for transient producers. Add a process-level test proving a one-shot
notification reaches a terminal audit outcome before successful command exit.

The real `thegn automations test` entry point opens/migrates state through
`host_config::merge_db_hosts` before calling the pure command. This was
reproduced with isolated XDG paths: the command created `thegn/thegn.db` before
returning `automation rule "missing" not found`. Dispatch this dry-run before
all DB-backed common setup (and do not install the runtime); add a process-level
test asserting an initially empty `XDG_STATE_HOME` remains empty.

`automations list` calls rules active when their live trigger/action requires a
daemon that is disabled or unavailable. Report a concrete inert reason for
daemon-only session/idle triggers and for catalog actions that cannot execute
without the daemon. Keep JSON and human output aligned.

## 5. Enforce action identity, ancestry, timeout, and outcome visibility

The default `ControlApi::tools_run` converts the request to `OpenSpec.agent`.
That resolver accepts `[[agents]]`, `[[tools]]`, and bare known harness ids, so
the public `tools.run` capability does not enforce its name-only `[[tools]]`
contract. Make the daemon implementation resolve exclusively against fresh
trusted `cfg.tools`; reject an agent entry or bare harness with the same name.
Likewise, automation `sessions.open` must resolve its `agent` against configured
agents rather than accepting a tool entry by accident. Pin both refusal cases.

All successful non-notify actions must produce one origin-tagged `automation`
outcome notification; failures/timeouts keep producing `automation_failed`.
Avoid a duplicate success row for `notify.push`, whose action notification is
already the visible result. Route both kinds through the canonical notification
seam.

Carry ancestry through every action-caused event. Notifications and sessions do
so now, but `merge.add` has no origin in its request/queue record, and the later
`MergeLanded` producers submit default facts. Persist/propagate the origin (or
otherwise structurally suppress that descendant) so a merge action cannot fire
a second rule. A timed-out `tools.run` must also terminate/kill the opened tool
session rather than merely cancel the wait future while the command continues.

## Required verification

Use fresh temporary `XDG_STATE_HOME` values for every command that may touch
state. Do not run a built binary against live state.

- `just quick thegn-core`
- `just quick thegn-svc`
- `just quick thegn-host`
- `cargo nextest run -p thegn-core automation`
- `cargo nextest run -p thegn-core env_overlay`
- current-main migration ladder and newer-schema filters
- `cargo nextest run -p thegn-svc --test control_schema`
- focused service catalog/route/scope tests for all three new verbs
- host automation runtime/event/action tests added above
- `cargo nextest run -p thegn-host notification`
- process-level isolated CLI tests for store-free dry-run and transient drain
- real-socket control/daemon filters in an environment that permits local
  Unix/TCP/WebSocket sockets
- `cargo fmt --all -- --check` and `git diff --check`

## Done criteria

- Current main is reconciled with a collision-free automation migration.
- Every documented trigger and selector has a truthful live producer; routed
  notification kind/priority and idle duration have independent semantics.
- Throttle/once/action admission and pre-dispatch audit are atomic across
  concurrent processes; overflow is durably audited and all state is bounded.
- One-shot producers cannot lose accepted events, and the CLI dry-run leaves an
  empty state directory empty.
- Catalog actions enforce their configured-name types, all descendants preserve
  ancestry, timed-out tools stop, and successful/failed outcomes are visible.
- The added tests exercise the real runtime and producer/control paths rather
  than only pure matching helpers.
