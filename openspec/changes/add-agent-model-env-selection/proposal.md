# Per-agent / per-stage harness, model, account and env selection

## Summary

An `[[agents]]` entry was `command` + `provider`; a pipeline stage could only
name an entry; the daemon snapshotted the registry at start. Choosing a model
meant baking flags into `command`, choosing an account meant restarting the
daemon under a different `CLAUDE_CONFIG_DIR`, and the pipeline could not move
to another harness (pi on the local model proxy) without killing every live
pane. Linear THE-83.

## What changes

- `NamedCommand` gains `model`, `env`, `permissions`; `harness` is an alias of
  `provider`. `PipelineStage` gains the same three as overrides.
- `Harness::model_flag` per harness; a `pi` harness (`pi -p {prompt}`).
- `agent_task::effective_agent` resolves entry + stage into the launch view;
  every launch path (TUI pane, daemon `sessions.open`, presets) goes through it.
- `session open --stage` / `AgentLaunch.stage`.
- `agent_permissions` seeds the harness's per-worktree allow-list at launch.
- `config_source` records the CLI's config source; the daemon reloads it per
  agent launch.
- `config validate` rejects a model on a flagless harness and bad env keys;
  `doctor` shows the effective view.

## Impact

tasks.md: pipeline v2 (THE-76 items 4, 6) and THE-83. Config keys are all
optional and default to today's behaviour.
