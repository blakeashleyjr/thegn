# Tasks — worktree lifecycle hooks

## 1. Core model (thegn-core, pure)

- [ ] 1.1 New `hooks.rs`: `HookEvent` enum (`pre_create`, `post_create`,
      `pre_destroy`, `post_destroy`, `session_start`, `session_end`),
      `HookEntry` (string or `{ command, wait, timeout_secs, on_failure }`),
      `HooksConfig` + workspace/repo overlays; scope accumulation in
      global → workspace → repo order; unit tests (95% core gate).
- [ ] 1.2 Failure-policy resolution: per-event defaults
      (block / warn / block-with-force / unattended-warn), `on_failure`
      clamped so repo-sourced entries are always warn-only; exhaustive table
      tests.
- [ ] 1.3 Env contract builder: curated base + `THEGN_*` context vars; unit
      test that the full process env is not inherited.
- [ ] 1.4 `prepare` alias: fold `[sandbox] prepare` entries as the head of
      `post_create` (warn, no wait); compatibility test.
- [ ] 1.5 `config_resolve.rs`: classify repo `[hooks]` as Gated with
      per-event `hooks.<event>` `GatedRequest` categories (canonical-form
      matched); extend the exhaustive-destructure classification tests.

## 2. Executor (thegn-host)

- [ ] 2.1 New `hook_run.rs`: `sh -lc` off-loop execution, per-hook timeout
      with process-group kill, output capture to
      `$XDG_STATE_HOME/thegn/hooks/<slug>/<event>-<n>.log`, failure
      notification with output tail, refresh-channel send +
      `TerminalWaker` pulse; processes join the shared slice via
      `wrap_background_argv`.
- [ ] 2.2 Retire `thegn_core::sandbox::run_prepare` into the executor
      (failures now surface; behaviour otherwise unchanged).

## 3. Call sites (thegn-host)

- [ ] 3.1 Create: `wizard.rs` worker — `pre_create` (blocking, before
      `worktree add`), `post_create` after built-in provisioning, `wait`
      honoured before first-pane spec compose; same for `cmd/wt.rs new`.
- [ ] 3.2 Destroy (user-invoked): sidebar delete / `cmd/wt.rs rm` /
      `handlers/workspace_remove.rs` — `pre_destroy` with block + force
      override, then `post_destroy`.
- [ ] 3.3 Destroy (unattended): `merge_lifecycle.rs` reclaim — `pre_destroy`
      warn-and-continue, then `post_destroy`.
- [ ] 3.4 Session boundaries: `session_start` on a worktree's first pane
      spawn of a session, `session_end` on last-pane exit / tab close
      (run.rs / pane lifecycle handlers), warn-only, `Background` QoS.
- [ ] 3.5 Trust plumb-through: pending `hooks.<event>` requests surface via
      the same `repo_trust` flow as other repo overlays.

## 4. Config + docs

- [ ] 4.1 Document `[hooks]`, `[workspace.<slug>.hooks]`, the repo
      `.thegn.toml [hooks]` gating, and the `prepare` alias in
      `config/config.toml.example` (the generated config-reference help page
      picks it up).
- [ ] 4.2 `cmd/doctor.rs`: hooks line — configured events, source scopes,
      repo trust state. No new catalog row.
- [ ] 4.3 Update the worktree help/docs pages for the delete-flow force
      override (any new modal copy stays within existing zones — no new
      action ids expected; if one is added, claim it on a docs/help page per
      the help ratchet).

## 5. Tests + gate

- [ ] 5.1 Smoke coverage: a hook that writes a marker file runs at create and
      destroy in `test/smoke.sh` (hermetic XDG_STATE_HOME).
- [ ] 5.2 Verify delta spec scenarios against the implementation.
- [ ] 5.3 Run `just ci` once (includes openspec-validate) when the
      implementation is complete.
