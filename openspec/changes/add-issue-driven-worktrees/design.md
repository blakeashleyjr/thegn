# Design

## The action (host)

The **`s` ("start")** key on a selected issue, in both the Issues and Mine
sections (the existing `b` branch-from-issue and `D` agent-dispatch keys remain
as today), runs a pipeline:

1. **Branch/worktree** — derive a branch from the issue via
   `worktree::issue_branch_name(template, ...)` (honoring
   `session_name_template`, e.g. `{identifier}-{slug}`), create it with
   `add_checked` via `begin_worktree_preset` with `NameSpec::Fixed`, and
   add+focus a `WorktreeGroup` via `session::add_group`.
2. **Bind** — record the issue↔worktree link in the existing `issue_links` table
   so the panel's linked-worktree marker (`◈`) lights up and the binding survives
   restart.
3. **Agent (optional)** — if `auto_launch_agent` is on, build a `launch_spec` and
   spawn the agent as a visible pane, seeding the issue's title/body into the
   agent's initial context (via the pane env / initial prompt path).

`auto_create_worktree` gates whether the action creates a worktree or just binds
to an existing one; `auto_launch_agent` gates step 3.

## Mechanism (host wiring)

The implementation generalizes the existing
`pending_issue_link: Option<(u64, String)>` mechanism in `run.rs` to
`pending_issue_start: Option<(u64, IssueStartCtx { issue_id, title, body, url,
launch_agent })>`, resolved when the worktree creation completes at
`CreateEvent::Done`. Worktree creation reuses `begin_worktree_preset` with
`NameSpec::Fixed`. Key handling lands in
`crates/thegn-host/src/handlers/tracker.rs`, not `run.rs` (god-file ratchet —
`run.rs` may only shrink).

The branch-name template function `worktree::issue_branch_name(template, ...)`
lives in **thegn-core** (moving/extending the host's
`naming.rs::issue_branch_tail`), with tokens `{identifier}`/`{slug}`/
`{provider}`.

## Context seeding (agent, additive)

The issue title/body/URL are passed to the agent as initial context, reusing the
existing `D`-dispatch body verbatim: `THEGN_ISSUE_ID`/`THEGN_ISSUE_TITLE`/
`THEGN_ISSUE_BODY`/`THEGN_ISSUE_URL` env vars plus the initial prompt, via
`direnv_warm::launch_spec_synced` → `attach_agent_pane`. This is the only
AI-touching part and is skipped entirely when no agent runs.

## Invariants

- **Event loop**: worktree creation + clone-like git work runs off-loop
  (spawn_blocking), handed back over the channel + `TerminalWaker`; agent spawn is
  the existing pane-spawn path. No polling timer, no blocking git on the loop.
- **Render**: adding/focusing the worktree tab is a chrome `dirty` repaint + normal
  tab activation. render_plan invariants unchanged.
- **State**: no `user_version` bump — reuse `issue_links`.
- **Additivity**: issue→worktree is shell-level and works with no agent; the
  agent-launch + seeding is the additive AI layer, gated by config.

## Alternatives considered

- **A new issue↔worktree table** — rejected; `issue_links` already models exactly
  this worktree-scoped binding.
- **Always launching an agent** — rejected; the worktree-creation half must stand
  alone for the AI-free shell, so the agent step is gated.
- **Seeding context by scraping the issue into a file** — the env/initial-prompt
  path is cleaner and matches how the pane firewall already injects context.
