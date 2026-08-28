# Chunk 3 — THE-84: the last shell paths stop overwriting the remembered agent

Closes THE-84 lane (3). THE-85's D4 suppressed the agent record on the
materialize/split/preset shell paths, but the audit (design.md §3) found two
remaining `launch_spec` callers that resolve `"shell"` with
`LaunchExtras::default()` — i.e. they hit the guarded write
(`agent.rs:3055-3066`) and record `shell` over a remembered agent:

1. **`crates/thegn-host/src/run.rs:5177`** — `prewarm_sandbox_chain`
   (kicked by ApplyLayout `run.rs:20342` and ImportLayout `run.rs:20362`):
   `let _ = crate::agent::launch_spec(&cfg, &wt, None, "shell");` — a sandbox
   warm-up whose spec resolution **writes** `worktrees.agent = "shell"` as a
   side effect. Also resolves the non-daemon-persistent variant
   (`launch_spec` hard-codes `daemon_persistent = false`), which is the wrong
   builder for a daemon-routed box (see `launch_spec_center`'s docs,
   `agent.rs:2961-2984` — the `--die-with-parent` vs pane-daemon contract).
2. **`crates/thegn-host/src/main.rs:1219`** — the `sandbox-argv` verb: a
   read-only debug print that resolves through `launch_spec(...)` ⇒ **writes**
   the record as a side effect of _reading_ the argv.

## Files touched (exact paths)

- `crates/thegn-host/src/agent.rs` — NEW tiny helper `prewarm_spec` beside the
  other launch builders (~15 lines + doc comment; agent.rs is at its god-file
  ceiling, so the helper is two calls long and lives with its siblings):
  ```rust
  /// The sandbox-chain pre-warm resolution (run.rs `prewarm_sandbox_chain`):
  /// daemon-routed like any center pane, and NEVER recorded — a warm is not a
  /// choice of agent (THE-84: it must not clobber `worktrees.agent`).
  pub(crate) fn prewarm_spec(cfg: &Config, worktree: &str) -> anyhow::Result<LaunchSpec> {
      launch_spec_center_with(cfg, worktree, None, "shell", LaunchExtras {
          suppress_agent_record: true, ..Default::default()
      })
  }
  ```
- `crates/thegn-host/src/run.rs` — `prewarm_sandbox_chain` body
  (`:5177`): `crate::agent::prewarm_spec(&cfg, &wt)` (the ignored `let _`
  stays — best-effort warm, comment already sanctioned).
- `crates/thegn-host/src/main.rs` — `Command::SandboxArgv` (`:1219`):
  `launch_spec_full(&cfg, &wt, None, "shell", false, false, LaunchExtras {
suppress_agent_record: true, ..Default::default() })` — read-only verb,
  read-only side effects.
- `crates/thegn-host/src/agent_tests.rs` — regression tests (this file is the
  shared test home for the agent record; no other chunk touches it).

## Approach

No behavior change beyond the record: the prewarm keeps resolving the same
sandbox argv (now via the center builder, which on a daemon-active box marks
the spec daemon-persistent — strictly more correct for the warm's purpose),
and the verb keeps printing the same argv. The ONLY delta is
`suppress_agent_record: true` ⇒ `set_worktree_agent` is not called
(`agent.rs:3061-3066`).

## Tests (scoped)

- `just quick thegn-host`
- `cargo nextest run -p thegn-host agent_tests`
- `cargo nextest run -p thegn-host prewarm`

Unit tests to add in `agent_tests.rs` (mirror the THE-85 D4 test at
`:474-527`; isolate `XDG_STATE_HOME` — the shell often runs inside a live
thegn):

1. `prewarm_spec_leaves_the_worktrees_agent_alone` — register a worktree row,
   `set_worktree_agent(wt, "claude")`, call `agent::prewarm_spec(cfg, wt)`
   (Ok and Err paths), assert `worktree_agent(wt) == Some("claude")` after.
2. `sandbox_argv_resolution_leaves_the_worktrees_agent_alone` — same shape
   asserting the `suppress_agent_record` extras on the full-launch call the
   verb now makes (call the same expression the verb uses; the verb itself is
   a thin CLI shell — no subprocess test, consistent with the crate's CLI
   testing policy).

## Done criteria

- [ ] All gates above green; `just quick thegn-host` clean.
- [ ] Audit invariant (grep-verifiable, cite in the commit body): every
      `launch_spec*` call site that passes `"shell"` now sets
      `suppress_agent_record: true` —
      `materialize.rs:204/:242`, `run.rs:5092/:7854` (chunk 1 keeps these),
      `run.rs:5177` (now via `prewarm_spec`), `main.rs:1219` — and the only
      unsuppressed writers left are the deliberate ones (wizard `:164`,
      preset bind `run.rs:10155`, `--bind` `agent_open.rs:118`, launch-menu
      choices `launch.rs:130-143`).
- [ ] `worktrees.agent` is only ever written by a user-visible choice or an
      explicit bind.
- [ ] Commit subject (exact):

```
fix(the-84): the last shell paths stop overwriting the remembered agent
```

## Overlap / dependency

Touches `run.rs` (one-line call swap) — **run AFTER chunk 2** (chunks 1–3 all
touch `run.rs`; strictly serial 1 → 2 → 3). `agent.rs` / `main.rs` /
`agent_tests.rs` are otherwise untouched by this lane. Chunk 3 does NOT
depend on chunk 1 or 2 symbols — the serialization is purely the shared file.
