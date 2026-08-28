# Chunk 3 done — THE-84: the last shell paths stop overwriting the remembered agent

Commit: `96cddd41` (`fix(the-84): the last shell paths stop overwriting the remembered agent`)

## What changed

- **`crates/thegn-host/src/agent.rs`** — new `pub(crate) fn prewarm_spec(cfg, worktree)`
  beside the other launch builders: `launch_spec_center_with(..., "shell",
LaunchExtras { suppress_agent_record: true, .. })` — daemon-routed like any
  center pane, never recorded. Also annotated plain `launch_spec` with
  `#[cfg_attr(not(test), expect(dead_code))]`: after the two call-site swaps it
  has **zero production callers** (test-only), and the `expect` is a tripwire
  that forces re-auditing of `suppress_agent_record` if a production caller
  ever reappears. (Needed: removing the last two callers made the fn dead under
  the crate's `-D warnings`; precedent `host_provision.rs:322`.)
- **`crates/thegn-host/src/run.rs`** (`prewarm_sandbox_chain`, ~:5180) —
  `launch_spec(&cfg, &wt, None, "shell")` → `prewarm_spec(&cfg, &wt)`. The
  ignored `let _` stays (best-effort warm, sanctioned). Bonus correctness per
  the spec: the warm now resolves the daemon-persistent variant on a
  daemon-routed box instead of `launch_spec`'s hard-coded
  `daemon_persistent = false` (`--die-with-parent` builder).
- **`crates/thegn-host/src/main.rs`** (`Command::SandboxArgv`, ~:1221) — now
  `launch_spec_full(..., "shell", false, false, LaunchExtras {
suppress_agent_record: true, .. })`: read-only debug verb, read-only side
  effects. Same argv and same `daemon_persistent = false` as before.
- **`crates/thegn-host/src/agent_tests.rs`** — two regression tests mirroring
  the THE-85 D4 shape (temp `XDG_STATE_HOME` via `with_temp_state` +
  `ENV_LOCK`, register row first since `set_worktree_agent` is UPDATE-only):
  - `prewarm_spec_leaves_the_worktrees_agent_alone` — asserts the remembered
    agent survives the **Ok** path _and_ the **Err** path (explicit-WSL
    no-fallback config; the record write in `launch_spec_full` precedes the
    failing sandbox resolution, so an unsuppressed failing warm would still
    have stamped the row — this pins the suppression, not luck).
  - `sandbox_argv_resolution_leaves_the_worktrees_agent_alone` — the exact
    expression the verb now evaluates; no subprocess test (crate CLI policy).

## Done criteria

- [x] `just quick thegn-host` clean (after the `expect(dead_code)` annotation).
- [x] `cargo nextest run -p thegn-host prewarm` — 5/5 passed.
- [x] `cargo nextest run -p thegn-host 'agent::tests::'` — 34/34 passed
      (includes both new tests and the THE-85 D4 pins, still green).
- [x] Audit invariant (grep-verified): every production `launch_spec*` call
      site passing `"shell"` sets `suppress_agent_record: true` —
      `materialize.rs:210/:248`, `run.rs:5098/:7873` (chunk 1's), `run.rs:5180`
      via `prewarm_spec`, `main.rs:1230`. Remaining `"shell"` `launch_spec(`
      hits are `#[cfg(test)]` only (panes.rs:1406/:1501).
- [x] `worktrees.agent` writers are now only: the guarded write in
      `launch_spec_full` (a user-picked agent launch), the wizard
      (`wizard.rs:164`), the preset bind (`run.rs:10186`), and `--bind`
      (`daemon/agent_open.rs:118`). (The `handlers/worktree_launch.rs:227` hit
      is a `#[cfg(test)]` helper.)
- [x] Commit subject exact: `fix(the-84): the last shell paths stop
overwriting the remembered agent`.

## Dev-loop compliance

`just quick thegn-host` + two targeted nextest filters only. No
`just test`/`ci`/`coverage`, no full-workspace compile, no e2e.

## Unverified

- **`just test` / pre-push gate** not run (policy): the rest of the
  `agent_tests`/`panes` suites beyond the two filters above were not executed
  locally. The only compile-visible fallout was the `launch_spec` dead-code
  error, resolved with the `expect` tripwire; a full `cargo test` could in
  principle surface something the two filters missed (low risk — the change is
  two call swaps + one annotation).
- **`just ci`-only gates** (coverage, doctests, openspec-validate, e2e) not
  run; e2e explicitly out of scope — no frame-altering change.
- **Runtime behavior of `prewarm_spec` under a live daemon** (`daemon_active`
  → `daemon_persistent = true` on the warm's spec) is asserted only by the
  builder's own logic, not by a test that exercises an active daemon route.
  Argv parity with the old `launch_spec` resolution (same argv, different
  persistence flag) follows from reading `launch_spec_center_with` →
  `launch_spec_full`, not from an execution check.
