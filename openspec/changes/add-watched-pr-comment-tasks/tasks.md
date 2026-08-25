# Tasks — watched-PR comment tasks

Depends on `add-pr-queue` (implemented; sync/archive first).

## 1. Pure classification (thegn-core)

- [ ] 1.1 `Blocker::UnresolvedComments(n)` (wire word `unresolved_comments`)
      with `watch_kind() = Review`, `task_kind() = PrReview`; extend
      `classify` inputs with the unresolved-thread summary; ordering
      `ChangesRequested > UnresolvedComments > ChecksPending` — **table
      tests**: ranking, count display, empty-threads ⇒ unchanged behavior
      (95% core gate).
- [ ] 1.2 `review_trigger` gating in `decide` (`changes_requested` default ⇒
      display-only; `any_unresolved` ⇒ dispatchable under the existing
      watch/own/budget/worktree rules) — **table tests** for both modes.
- [ ] 1.3 Fingerprint over sorted unresolved thread ids + refill rule
      (refill only on a thread id not previously seen; agent replies inert)
      — **unit tests**: new-id refill, resolve-one-open-one changes, reply
      no-op, fetch-failure keeps previous.

## 2. Config (thegn-core)

- [ ] 2.1 `[pr_queue] review_trigger` `config_enum!`
      (`changes_requested` | `any_unresolved`), overlay destructuring,
      round-trip test, documented in `config/config.toml.example`.

## 3. Persistence (thegn-core)

- [ ] 3.1 Additive `pr_queue` columns `agent`, `agent_command`,
      `threads_fingerprint` + **`user_version` bump** + migration; store
      methods and `PrQueueRow` (`--json`) extended — migration-ladder +
      CRUD tests.

## 4. Driver + poller (thegn-host)

- [ ] 4.1 Poll-path thread fetch for open non-draft rows under the existing
      per-row backoff; fingerprint update + budget refill + re-evaluate.
- [ ] 4.2 Dispatch resolution order row-`agent_command` > row-`agent` >
      config, through `agent_task::resolve_agent` + template validation;
      unresolvable override ⇒ `needs_human` with reason.
- [ ] 4.3 New notification kind for new feedback on a watched entry (fires on
      fingerprint gain, foreign-author rows included).

## 5. Surfaces (thegn-host)

- [ ] 5.1 `pr queue add --agent/--agent-command` flags (existing catalog row;
      `--json` list reports the override).
- [ ] 5.2 Panel `prq` row action to set/clear the override from the
      configured `[[agents]]` list; blocker cell renders the unresolved
      count.

## 6. Help + docs + validation

- [ ] 6.1 `docs/help/pr-queue.md` (context `panel:prq`): `review_trigger`,
      the override action/flags, the new notification — help + prose
      ratchets; claim any new action id.
- [ ] 6.2 Document the public-repo prompt-injection consideration of
      `any_unresolved` alongside the config key.
- [ ] 6.3 Run `just ci` once, pre-PR (includes `openspec validate --all
--strict`).
