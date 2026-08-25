# Tasks

## 1. Core engine (thegn-core, pure)

- [ ] 1.1 `automation.rs`: `AutomationEvent`, `Rule`, `Planned`,
      `evaluate(rules, event, state, now)`, throttle ledger transitions,
      one-generation tag check, schedule `due()` — **unit tests**: selector
      tables (kind/glob/source/regex/priority), edge vs level (same-state
      re-broadcast never fires), cooldown/max_per_hour drops, tagged events
      never match, `at`+`days` and `every` due-ness incl. no-catch-up and
      skip-while-running (95% gate).
- [ ] 1.2 Template rendering for all four actions through the agent-task
      substitution + `validate_template` quoting contract — **unit tests**:
      unknown var rejected, quoted placeholder in a command template
      rejected, prompt never shell-quoted.
- [ ] 1.3 `config_automations.rs`: `[automations]` + `[[automations]]`
      parsing/validation (unique names, exactly-one-action, regex
      precompile, `every` floor clamp, `invoke` scope-ceiling check at load)
      — **unit tests** + `config/config.toml.example` entries for every key.
- [ ] 1.4 Trusted-layers-only: repo overlay carrying automations is stripped
      with a surfaced warning — **unit test** on the config merge.
- [ ] 1.5 `TaskKind::Automation` (vars, default prompt, validation) — update
      the pinned-count tests a new kind trips.
- [ ] 1.6 DB: `automation_runs` + `automation_state`, `user_version` bump
      (pick the number at land time; check sibling bumps), store methods +
      retention sweep — **unit tests** via `db_tests` conventions.

## 2. Wiring (thegn-host)

- [ ] 2.1 Notification tap in `notify::record` (UI) and the daemon
      `notify_push` path — evaluate after user rules (dropped rows invisible
      to automations), enqueue `Planned` to the worker; never propagate
      errors to the record caller.
- [ ] 2.2 Daemon: session-FSM/exit tap on the edge broadcast + schedule
      ticker task (tokio, QoS Background); inert with a doctor/`list` note
      when the daemon is disabled.
- [ ] 2.3 Action worker: bounded queue, `max_concurrent` pool,
      `wrap_background_argv` for `run`, headless `agent_run` for `task`,
      catalog dispatch for `invoke`, `notify::record` (tagged source) for
      `notify`; timeouts; audit rows on every outcome; `automation_failed`
      notification on failure; drop-on-overflow audited. Waker-pulsed status.
- [ ] 2.4 One-generation stamping: `automation:` source prefix +
      `THEGN_AUTOMATION_ORIGIN` on spawns; taps filter tagged events.
- [ ] 2.5 New notification kinds `automation` / `automation_failed` — kind
      enum, default priorities, example-prose pinned test.

## 3. Surfaces

- [ ] 3.1 Catalog: `automations.list` / `automations.audit` (Read),
      `automations.set_enabled` / `automations.run` (Write) — Verb +
      `required_scope` + `CATALOG` + `ROUTES`; gRPC mirrored or
      `SURFACE_GAPS`; MCP names via `CapId::tool_name` (Write tools ride the
      in-flight `--scopes` gating).
- [ ] 3.2 `cmd/automations.rs`: `list | audit | enable | disable | run
<name> [--dry-run]` with `--json`; graceful no-daemon degradation.
- [ ] 3.3 `docs/help/automations.md` claiming the new action ids (help +
      prose ratchets green); config-reference page picks up the new table
      automatically.
- [ ] 3.4 Doctor: automations summary (rule count, daemon-dependent triggers
      active/inert, scope ceiling).

## 4. Verification

- [ ] 4.1 Smoke: a `notification`-triggered `notify` rule fires end-to-end
      in a hermetic `XDG_STATE_HOME`; dry-run records and does not act;
      repo-overlay rules are refused.
- [ ] 4.2 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
