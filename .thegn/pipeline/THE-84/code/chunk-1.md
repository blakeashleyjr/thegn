# Chunk 1 — THE-84: a resurrected tab relaunches its remembered agent (resume-aware)

Implements `openspec/specs/agent/spec.md` ("The worktree remembers its agent"
→ "session resurrection relaunches the remembered agent"), which is contracted
but unimplemented: both spec-resolving workers hard-code `"shell"`
(`crates/thegn-host/src/handlers/materialize.rs:204-212` and `:242-250`;
prewarm worker `crates/thegn-host/src/run.rs:7854-7862`). See
`.thegn/pipeline/THE-84/architect/design.md` §2 for the evidence trail.

## Files touched (exact paths)

- `crates/thegn-host/src/handlers/worktree_launch.rs` — **NEW** module: the
  relaunch decision + its unit tests (keeps materialize.rs and run.rs at
  call-site-only edits, per the god-file guidance).
- `crates/thegn-host/src/handlers/mod.rs` — one `pub(crate) mod
worktree_launch;` line (alphabetical, near `worktree_attach`).
- `crates/thegn-host/src/handlers/materialize.rs` — after the shell specs
  resolve in BOTH shell branches (warm-spare `:204-212`, post-provision
  `:242-250`), apply the relaunch override.
- `crates/thegn-host/src/run.rs` — prewarm worker (`:7854-7862`): same
  override after its shell resolve. **This is the chunk's ONLY run.rs hunk.**
- `crates/thegn-host/src/daemon/agent_open.rs` — `fn command_for` →
  `pub(crate) fn command_for` (visibility only; body untouched).

## Approach

New pure-ish decision fn (DB read + optional bounded fs walk; off-loop
callers only — the workers already run in `spawn_blocking`):

```rust
pub(crate) fn remembered_agent_relaunch(
    cfg: &Config, worktree: &str, leaf: u32,
) -> Option<(u32, LaunchSpec)>
```

Decision ladder — every gate fails open to `None` (= today's shell):

1. `thegn_core::db::Db::open().ok()?.worktree_agent(worktree)` → name.
   `None` | empty | `"shell"` | `"clean-shell"` | a tool drawer
   (`cfg.tool_command(name).is_some()`) ⇒ `None` (same exclusions as the
   native-exec path, `panes.rs:862-868`).
2. Entry no longer configured (`cfg.agent_command(name).is_none()`) ⇒ `None`;
   leave the stale record alone (the sidebar keeps attributing until the user
   re-adds the agent — a shell pane is still the honest spawn).
3. Compose the spec:
   `crate::direnv_warm::launch_spec_synced_with(cfg, worktree, None, name,
LaunchExtras { cmd_override, prompt: None, suppress_agent_record: true,
stage: None })`. Full sandbox/credential/cap parity by construction — the
   same shape `daemon/agent_open.rs::resolve` uses (`:94-113`). The second
   bounded direnv warm is a cached no-op (the shell resolve just warmed it —
   `direnv.rs:34` bounds a cold one at 20s).
4. `cmd_override`: only when the entry opted in — read `cfg.agents`/`cfg.tools`
   for `entry.resume` FIRST (cheap; the fs walk must never run for non-opted
   entries). When true: `thegn_svc::sessions::discover(cfg,
&SessionFilter { worktree: Some(worktree), harness: None },
&known_worktrees)` (bounded walker, `thegn-svc/src/sessions.rs:41-129`,
   `MAX_SESSIONS = 500`; `known` from `db.worktrees()`, empty-set on DB miss)
   → newest record's id → `thegn_core::agent_task::auto_resume_id(cfg, name,
Some(id))` (`agent_task.rs:601-618` — re-checks `resume`, the harness
   `RESUME` cap, and `session_id_ok`). `Some(id)` ⇒
   `daemon/agent_open::command_for(cfg, name, "", false, Some(id), None)`
   (id-shape-validated, refuses non-RESUME harnesses — `agent_open.rs:133-161`).
   Any `None` ⇒ cold launch (no `cmd_override`).
5. `suppress_agent_record: true` always — resurrection is not a choice event;
   the record already holds this agent, and a relaunch must never be able to
   change it.

Call-site shape (identical at all three sites):

```rust
let mut resolved = /* existing shell resolution */;
if attach.is_empty() && !quiet
    && let Ok(specs) = &mut resolved
    && let Some((leaf, spec)) = worktree_launch::remembered_agent_relaunch(&cfg, &wt, first_leaf)
    && let Some(slot) = specs.iter_mut().find(|(id, _)| *id == leaf)
{
    slot.1 = spec;
}
```

Gates: `attach.is_empty()` — the THE-85 probe (`materialize.rs:252-260`,
`run.rs:7864-7874`) already ran beside the resolve, so a LIVE session still
wins and the agent is never doubled; `!quiet` — a split/add into a tab that
already has a live pane stays a shell (a split is a shell gesture). `leaf` =
the FIRST missing leaf in tree order (`missing[0]`; the primary leaf, matching
the original spawn shape where the agent occupied the center pane).

Properties the coder must preserve:

- The relaunched session is worktree-tagged end-to-end (`spawn_argv_env` →
  `LazyDaemonSource.worktree` = cwd → `OpenSpec.worktree` →
  `SessionMeta.worktree`), so the NEXT open ATTACHes to it via THE-85 instead
  of relaunching again. Do not add a second dedup mechanism.
- No config keys, no help-page changes (no new action ids — the help ratchet
  is untouched), no spec deltas, no color/glyph literals, no platform cfg, no
  new ignored `Result`s (DB reads are Option-chained; the walker never errors).
- The daemon-open parity test in `agent_open.rs` tests must keep passing —
  the visibility bump changes nothing else.

## Tests (scoped — no full-workspace gates while iterating)

- `just quick thegn-host`
- `cargo nextest run -p thegn-host worktree_launch`
- `cargo nextest run -p thegn-host materialize`
- `cargo nextest run -p thegn-host agent_open` (visibility bump is inert)

Unit tests to add (in `worktree_launch.rs`'s `#[cfg(test)]` mod; isolate
`XDG_STATE_HOME` per CLAUDE.md — the shell often runs inside a live thegn):

1. `shell_record_never_relaunches` — record `"shell"` / empty / missing row ⇒
   `None`.
2. `tool_drawer_record_never_relaunches` — record `yazi` (a `[[tools]]` entry)
   ⇒ `None`.
3. `unconfigured_agent_falls_back_to_shell` — record names an agent absent
   from config ⇒ `None` (and the DB row is unchanged afterwards).
4. `remembered_agent_relanches_cold_by_default` — configured entry with
   `resume = false` ⇒ `Some`, argv carries the entry's interactive command,
   and NO harness-home walk happened (assert via a session home that would
   poison the result if read).
5. `resume_composes_the_harness_resume_form` — entry `resume = true`, harness
   with RESUME (claude), a seeded session home whose newest transcript records
   the worktree cwd ⇒ argv contains `--resume <id>` (id shape-validated).
6. `resume_without_a_session_launches_cold` — `resume = true`, empty session
   store ⇒ cold argv.
7. `the_record_is_never_written_by_a_relaunch` — pre-set a DIFFERENT agent in
   `worktrees.agent` than the entry name resolved… (gate 1 makes this
   unreachable; instead) assert `suppress_agent_record` semantics: relaunch
   with record `A` for entry `A` leaves the row byte-identical (mirror of
   `agent_tests.rs:474-527`).

Worker-level test (in `materialize.rs`'s test mod or `worktree_launch.rs`):
a config with one agent, a registered worktree row, probe forced empty —
the resolved batch's FIRST leaf carries the agent argv, the remaining leaves
carry the shell argv; with a non-empty `attach` the batch is untouched.

## Done criteria

- [ ] All gates above green; `just quick thegn-host` clean (no new clippy
      warnings; `run.rs` hunk limited to the one call site).
- [ ] Behavior: with a remembered agent and no live daemon session, opening
      (or prewarming) the worktree's tab spawns the agent (resumed when
      opted-in + resume-capable + a session exists); with a live session it
      attaches (unchanged THE-85); with `shell`/tool/nothing remembered it
      spawns a shell (unchanged).
- [ ] `worktrees.agent` is byte-identical before/after any relaunch.
- [ ] Commit subject (exact):

```
feat(the-84): a resurrected tab relaunches its remembered agent (resume-aware)
```

## Overlap / dependency

Touches `run.rs` (one hunk). Chunks 2 and 3 also touch `run.rs` ⇒ **serial:
run this chunk FIRST, then chunk 2, then chunk 3.** No dependency on their
symbols; the order is purely file-disjointness.
