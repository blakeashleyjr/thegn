# Add the Lead tab

Linear: THE-2171, THE-2173. Roadmap: tasks.md group BA (item 516).

## Problem

Operators want one long-lived agent that watches every worktree and drives
workers, without handing it a host shell or the daemon's implicit-admin socket.

## Change

A reserved terminal (`name = "lead"`, `kind = "lead"`) rendered in its own
sidebar section, running a sandboxed agent configured by `[lead]`.

## Decisions stated loudly

- No new `GroupKind` variant: the Lead is a `terminals` registry row. Each tab is
  a separate conversation; panes are daemon-owned.
- The privilege boundary is real: the sandbox masks the daemon socket and run
  dirs, and the Lead's MCP uses a scoped token, not local admin.
- The sandbox fails closed: no bwrap means no Lead, never a host shell.
- On stable, `Feature::Lead` clamps `[lead]` to none.

## Impact

Specs: sidebar, sandbox, config.
