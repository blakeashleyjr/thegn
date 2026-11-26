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

- Owner decision 2026-11-25: the Lead is allowed on the stable channel when the user sets it up
  explicitly through the flow, labelled "preview". The smallest safe mechanism is a `[lead] preview`
  marker the flow writes; the stable clamp keeps `[lead]` only when it is `true`. A hand-written
  `[lead]` without it stays clamped off on stable, so nothing changes for anyone who never runs the
  flow. The flow has no relaunch step and never forces `THEGN_CHANNEL=dev`.
- The doctor's Lead checks run in the background and print into the flow's last step; the restart
  confirm counts live daemon sessions and says how many would end.
- A "Lead — set up" sidebar row (both channels) and a welcome-tour step open the same flow.
- The login runs the existing `thegn lead login` verb in a pane, so its isolation guarantees are
  unchanged and unreimplemented.
- The flow is a pure state machine over on-disk facts, so it resumes after any step.

## Impact

Specs: lead. Code: `lead/setup.rs`, `lead/group.rs`, `menu.rs`, `config_write::remove_key`,
`config_lead.rs` (`preview`), `config_resolve.rs` (clamp), `sidebar.rs`, `welcome.rs`,
`cmd/doctor_lead.rs`.
