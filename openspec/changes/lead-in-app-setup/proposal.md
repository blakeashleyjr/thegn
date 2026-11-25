# Lead in-app setup

Linear: THE-2725.

## Why

Setting up the Lead needed a hand-edited config, `thegn daemon stop`, `thegn lead login` in a
terminal, `thegn doctor` and then Alt i. The owner wants the Lead set up without CLI commands.

## What Changes

Pressing the Lead jump key when `[lead]` is absent (or the Lead is not logged in or not attached to
the daemon) opens a guided setup: agent choice, prerequisite checks, a previewed config write, a
daemon restart, the agent login in a thegn pane, MCP logins, doctor checks, then open the Lead. A
reset row removes `[lead]`.

## Decisions stated loudly

- The Lead stays dev-channel only. On stable the flow explains this and prepares the config; it does
  not change the clamp. Recommendation: keep it dev-only until the Lead is verified live.
- The login runs the existing `thegn lead login` verb in a pane, so its isolation guarantees are
  unchanged and unreimplemented.
- The flow is a pure state machine over on-disk facts, so it resumes after any step.

## Impact

Specs: lead. Code: `lead/setup.rs`, `lead/group.rs`, `menu.rs`, `config_write::remove_key`.
