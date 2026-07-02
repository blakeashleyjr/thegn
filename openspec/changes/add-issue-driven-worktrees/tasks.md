# Tasks

## 1. Session-name template (superzej-core)

- [ ] 1.1 Extend branch/session naming to honor a `session_name_template`
      (e.g. `{identifier}-{slug}`) over an issue's fields via the existing
      `worktree::branch_name`/`custom_cmd` expansion — **unit tests**: template
      expands from issue fields, missing fields defaulted, collisions avoided,
      invalid chars sanitized.

## 2. Issue → worktree action (superzej-host)

- [ ] 2.1 Add a "start" action to the My Work panel over a selected `WorkRow`:
      create the worktree off-loop (`add_checked`), add+focus the group
      (`session::add_group`), and record the `issue_links` binding — **render
      test**: opening the tab is a chrome repaint; gated by `auto_create_worktree`.

## 3. Optional agent launch + context seeding (superzej-host)

- [ ] 3.1 When `auto_launch_agent` is on, spawn the agent via `launch_spec` as a
      visible pane, seeding the issue title/body/URL into initial context (pane env + initial prompt). Skipped entirely when off or no agent is configured.

## 4. Config + docs + validate

- [ ] 4.1 Add `[issues] auto_create_worktree` / `auto_launch_agent` /
      `session_name_template` config with the AI-free defaults documented in
      `config/config.toml.example` + the work-surface doc section.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
