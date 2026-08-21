# Shared agent-task engine (customizable prompts, `[[agents]]` by name)

## Summary

The merge queue's headless agent handoff works, but three things about it are
hardcoded in a way that blocks reuse and blocks users:

1. **The prompt is not customizable.** `merge_driver::build_prompt` composes a
   fixed string in Rust. A user who wants the fixing agent to follow their repo's
   conventions (run a formatter, keep a changelog, never touch generated files)
   has no way to say so.
2. **`agent_command` is disconnected from `[[agents]]`.** You configure `claude`
   once as an `[[agents]]` entry, then re-type a command string — with the right
   headless flags, which differ per agent — into `[merge_queue] agent_command`.
3. **The dispatch machinery is welded to the merge queue.** `run_agent` /
   `substitute` / `build_prompt` are private to `merge_driver.rs`, `#[cfg(unix)]`,
   and named after merge concepts (`THEGN_MERGE_PROMPT`). A second queue that
   needs a headless agent would have to copy all of it, including the Windows
   stub.

This change extracts one **agent-task engine**: a pure `thegn-core` module that
renders a configurable prompt template from a task's variables and resolves which
command to run, plus a host module that runs it under the existing process-group
watchdog. The merge queue is retrofitted onto it with **byte-identical default
prompts**, so behavior is unchanged unless a user opts in.

It also fixes a real defect found while mapping this: the shipped example
`agent_command = 'claude -p "{prompt}" …'` (`config/config.toml.example:2800`)
quotes a placeholder that `substitute` already shell-quotes via `util::sh_quote`,
so the agent receives a prompt wrapped in literal `'` characters. The code's own
doc (`merge_driver.rs:370-372`) says to use bare placeholders; the example
contradicts it.

## Impact

- Roadmap: **T 758** (agent-driven merge-queue driver) — this is the
  configurability and reuse half of that item. It is also the enabling
  prerequisite for the team-facing PR queue (**Z 338/340**, **AT 646**), which
  needs exactly this dispatch layer and must not fork it.
- Spec: `merge-queue` — MODIFIED agent-handoff requirement (prompts are
  template-driven; the agent may be named). `agent` — ADDED requirement that a
  configured `[[agents]]` entry can be resolved to a headless command.
- Code: new `thegn-core/src/agent_task.rs` (pure), new
  `thegn-host/src/agent_run.rs`; `merge_driver.rs` loses `build_prompt` /
  `substitute` / `run_agent`. New optional `[merge_queue] agent` +
  `[merge_queue.prompts]` keys.
- **No DB schema change.** No new action ids, keybinds, zones, or panel sections,
  so the help ratchet is untouched; the generated config-reference page picks up
  the new keys automatically from `config.toml.example`.

## Rationale

thegn is an AI-free shell whose one agent-adjacent seam is "run an arbitrary
configured command". That seam is the right place to invest: making the prompt
and the command **data** rather than Rust keeps the shell itself AI-free while
making the feature work with any agent, present or future. Hardcoding a prompt
is the opposite — it bakes one vendor's ergonomics into the compositor.

Extracting it now, rather than when the PR queue lands, means there is exactly
one place where thegn spawns a headless agent: one quoting contract, one
watchdog, one env contract, one Windows port to do later.

## Non-goals

- **Changing what the merge queue decides.** Fold, gate, CAS, bisect, attempt
  budgets, and status transitions are untouched. Only _how the agent is invoked_
  moves.
- **A prompt library or prompt "packs".** One template per task kind, in config.
- **Making `run_agent` work on Windows.** It stays stubbed; this change only
  ensures the stub exists in one place instead of two. Building argv rather than
  a shell string is groundwork, not the port.
- **The PR queue itself.** That is a separate change built on this one.
- **Interactive agents.** `[[agents]]` entries keep launching interactively in
  panes exactly as they do today; this only adds a headless _resolution_ path.
