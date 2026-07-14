# Tasks

## 1. Session-name template (thegn-core)

- [ ] 1.1 Add `worktree::issue_branch_name(template, ...)` in thegn-core
      (moving/extending the host's `naming.rs::issue_branch_tail`) honoring
      `session_name_template` with tokens `{identifier}`/`{slug}`/`{provider}`
      — **unit tests**: template expands from issue fields, missing fields
      defaulted, collisions avoided, invalid chars sanitized (95% gate).

## 2. Issue → worktree action (thegn-host)

- [ ] 2.1 Add the `s` ("start") action over a selected issue in both the Issues
      and Mine sections (existing `b`/`D` keys unchanged), with key handling in
      `crates/thegn-host/src/handlers/tracker.rs` (not `run.rs` — god-file
      ratchet): generalize `pending_issue_link` to `pending_issue_start:
      Option<(u64, IssueStartCtx { issue_id, title, body, url, launch_agent })>`
      resolved at `CreateEvent::Done`; create the worktree off-loop via
      `begin_worktree_preset` with `NameSpec::Fixed`, add+focus the group
      (`session::add_group`), and record the `issue_links` binding — **render
      test**: opening the tab is a chrome repaint; gated by `auto_create_worktree`.

## 3. Optional agent launch + context seeding (thegn-host)

- [ ] 3.1 When `auto_launch_agent` is on, spawn the agent as a visible pane
      reusing the existing `D`-dispatch body: `THEGN_ISSUE_ID`/`TITLE`/`BODY`/
      `URL` env + initial prompt via `direnv_warm::launch_spec_synced` →
      `attach_agent_pane`. Skipped entirely when off or no agent is configured.

## 4. Config + docs + validate

- [ ] 4.1 Add `[issues] auto_create_worktree` / `auto_launch_agent` /
      `session_name_template` config with the AI-free defaults documented in
      `config/config.toml.example` + the work-surface doc section.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
