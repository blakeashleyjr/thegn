# Tasks — issue autopilot

Depends on `add-pr-queue` (implemented) and `add-agent-task-engine`
(implemented). Composes with `add-issue-driven-worktrees` /
`add-generic-tracker-model` — reconcile naming/transitions with whichever has
landed at implementation time.

## 1. Task kind (thegn-core)

- [ ] 1.1 `TaskKind::IssueImplement` with prompt vars (`issue_id`,
      `issue_number`, `issue_title`, `issue_body`, `issue_url`, `branch`,
      `base`, `worktree`) and a built-in prompt carrying the merge-family
      rules (commit, do NOT push, never merge) — **unit tests**: default
      renders/validates; extend the pinned-count/kind tests.

## 2. Config (thegn-core)

- [ ] 2.1 `config_autopilot.rs`: `[autopilot]` — `enabled` (false),
      `trigger_label`, `assignee` (`me`), `pickup_status` (todo),
      `max_concurrent` (1), `max_attempts` (1), `open_as`
      (`ready` | `draft`), `comment_on_pickup` (false), `done_on_merge`
      (true), `agent`/`agent_command`, `[autopilot.prompts]` — overlay,
      validation, round-trip tests; every key documented in
      `config/config.toml.example` with the trust-boundary warning on
      `enabled`.

## 3. Pure pickup policy (thegn-core)

- [ ] 3.1 `autopilot.rs`: `matches(issue, cfg)`, `claimable(…)` (dedupe
      against runs, concurrency cap, oldest-first), run state machine
      (`claimed → working → pr_opened → shepherding → done | needs_human |
stopped`) — **exhaustive table tests** (95% gate): label/assignee/
      status gating, cap, duplicate suppression, legal transitions,
      retry-only-from-terminal, attempt budget.

## 4. Persistence (thegn-core)

- [ ] 4.1 `autopilot_runs` DDL (unique issue id) + **`user_version` bump** +
      additive migration; store methods (claim/update/list/terminal) +
      `AutopilotRunRow` (`--json`) — CRUD + migration-ladder tests + the
      duplicate-claim-refused test.

## 5. Run driver (thegn-host)

- [ ] 5.1 Pickup hook on issue-refresh completion (off-loop, skipped while
      disabled): evaluate + claim + spawn run drivers on `sched::spawn_bg`
      (Background QoS), waker pulse per settled step.
- [ ] 5.2 Run pipeline: worktree/branch from the issue (shared naming path,
      `issue_links` bind) → tracker `in_progress` (+ optional comment;
      failures = run note) → headless dispatch via `agent_run` (watchdog,
      sandbox slice) → validate clean/ahead → plain push (never force) →
      `forge::create_pr` (issue-derived title/body, `open_as`) → enqueue into
      the PR queue (or stop at pr_opened with a note when disabled).
- [ ] 5.3 Failure paths: timeout/dirty/no-commits ⇒ `needs_human` +
      notification, worktree preserved, no tracker write; crash-restart
      resurfaces the run as `needs_human`.
- [ ] 5.4 Merge observation: the PR queue's settled `merged` transition for
      an autopilot PR ⇒ tracker `done` under `done_on_merge`; run ⇒ `done`.

## 6. Surfaces (thegn-host)

- [ ] 6.1 `cmd/autopilot.rs`: `status`/`stop`/`retry` with `--json`;
      **CATALOG rows + `required_scope`** for each verb (the gaps list only
      shrinks); refuse-with-guidance while disabled.
- [ ] 6.2 Notifications (picked up / PR opened / needs human / done) and the
      run badge on Issues/Mine rows (glyph via caps chokepoint).

## 7. Help + docs

- [ ] 7.1 New `docs/help/autopilot.md` claiming the verbs' action ids and
      documenting the loop, the never-without-a-human rules, and the
      label-as-consent trust boundary (help + prose ratchets).

## 8. Validation

- [ ] 8.1 Smoke: hermetic end-to-end of the CLI verbs against an isolated
      `XDG_STATE_HOME` (no live tracker/forge — seams faked as in the
      pr-queue driver tests).
- [ ] 8.2 Run `just ci` once, pre-PR (includes `openspec validate --all
--strict`).
