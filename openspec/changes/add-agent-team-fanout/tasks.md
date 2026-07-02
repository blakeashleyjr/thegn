# Tasks

## 1. Core fan-out plan (superzej-core)

- [ ] 1.1 `team.rs`: pure `plan_team(spec) -> TeamPlan` mapping `--agents` /
      `--best-of-N` into a list of `{ branch_name, agent, sandbox }` teammates
      (branch = `team/<label>/<agent-or-idx>`) — **unit tests**: heterogeneous
      list, best-of-N same-agent list, branch-name uniqueness, label defaulting.

## 2. Fan-out execution (superzej-host)

- [ ] 2.1 `szhost team <task> [--agents|--best-of-N|--agent] [--base] [--sandbox]
[--label]`: for each teammate create the worktree (existing create path),
      enter the sandbox via `sandbox::enter_argv`, launch the agent with the task;
      keep the caller's pane as orchestrator. Pull warm spares from the pool when
      available. All off-loop with channel + `TerminalWaker` reporting.
- [ ] 2.2 `team_label` grouping over the resulting worktree tabs (existing session
      grouping, no schema bump); teammates laid out as sibling panes via CenterTree.

## 3. Fleet roster (superzej-host)

- [ ] 3.1 Render the team as a fleet roster (one row per teammate: branch +
      activity/attention state + tokens/cost when the proxy provides them),
      reusing the sidebar row + activity-dot rendering — **render test**: roster
      updates are a chrome repaint; team creation is a `Full` frame (geometry).

## 4. Review & merge wiring (superzej-host)

- [ ] 4.1 Ensure teammate branches flow into the existing diff/review pane,
      needs-attention jump, cycle-through-diffs, and approve→merge /
      reject→discard; `--best-of-N` presents the N sibling diffs. Discarding a
      teammate reuses worktree cleanup (dirty guard).

## 5. Docs + validate

- [ ] 5.1 Document `szhost team` (both modes) in the CLI/agent doc section.
- [ ] 5.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
