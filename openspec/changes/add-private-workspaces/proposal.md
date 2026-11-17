# Durable private-copy workspaces with explicit, journaled sync

## Summary

THE-791 (owner decision 2026-10-13, programme rulings 2026-11-17) makes a private copy of the workspace the
default for untrusted work: an independent Git repository and file tree that the sandbox owns, that only
reviewed file changes ever leave, and that survives compute recreation. This change specifies that capability.
It is the contract that THE-791 gates 2, 3 and 4 are implemented against (THE-793 capture, THE-794
quarantine export and journaled sync, THE-796 conversion). It changes no code itself. The design source is
`docs/audits/sandbox-security-2026-10-13/design.md`; no requirement here adds scope beyond it and the gate
acceptance text.

This change replaces the earlier private-clone design of the programme (a linked clone selected by
`[sandbox.git] workspace = "private-clone"`). That key is deleted before it ships. The feature gate is
`Feature::PrivateCopy` (id `private_copy`); no `private-clone` or `private_clone` name survives.

### Naming: workspace versus private workspace

`openspec/specs/workspace/spec.md` already exists and describes a **workspace** as a repository entry in the
sidebar (create, delete, zones, hooks). A **private workspace** is a different thing: a private-copy checkout
of one worktree, with its own repository, baseline and journal. The new capability is therefore named
`private-workspaces` so the two cannot be confused. Nothing here modifies the `workspace` capability.

## What changes

- A `workspace.mode` choice (`private-copy`, `git-worktree`, `live-mount`) that is independent of the
  execution backend, with defaults per launch class (interactive panes keep `git-worktree`).
- Workspace records with a random identity, split storage (runtime-writable repository versus broker-owned
  metadata) and a lifecycle state machine that outlives any runtime.
- Explicit capture of dirty tracked, staged, unstaged and non-ignored untracked files into a fresh private
  repository that imports only the base commit, with an immutable file baseline held outside the workload.
- Quarantine export of files, a patch review built from broker-owned blobs, and a baseline/sandbox/host
  (B/S/H) sync that applies nothing on conflict, journals every step and recovers from a crash.
- Discard as a separate explicit action; credentials expire independently of file retention.
- Explicit conversion of existing workspaces that keeps the old tree until the user confirms.

## Non-goals

- The launch classes, the single admission handle, the adapter contract and the refusal table (change
  `add-launch-admission`).
- Network presets and egress (workstream WS04), and image or devcontainer building.
- Defaulting interactive panes to private-copy. The user's own interactive panes keep the `standard` profile
  and `git-worktree` (owner decision 2026-11-05). Only untrusted launch classes default to private-copy, and
  only after the gate-5 flip (WS11-P6-05).
- Hardening of `git-worktree` mode (workstream WS11 git hardening, P3-01 to P3-10) and the git write policy
  (`[sandbox.git] mode`), which govern `git-worktree` and `live-mount` only.
- A remote transport. A remote private-copy keeps its repository on a durable remote volume and returns files
  through the same exporter over the connector (WS09-P6-01).

## Impact

- **Roadmap:** `tasks.md` group BA (sandbox + remote programme), item 761; programme items WS11-P3-06s,
  P3-06a, P3-06b, P3-06c, P3-07, P3-17, P3-18, P3-19 (owners per `tasks.md` of this change).
- **Depends on:** `add-launch-admission` (storage admission, `LaunchAdmission`), WS00-P1-13 and WS00-P1-14
  (type stubs), WS00-P1-01 (`Feature::PrivateCopy`).
- **Archive:** at the end of the wave that completes it (B6). It only ADDs requirements to a new capability,
  so it conflicts with no other in-flight change.
- **Schema:** the `private_workspaces` table and `worktrees.workspace_mode` ride the W4 migration (B2). This
  change adds none.

### Acceptance coverage (THE-792 to THE-796)

Every acceptance bullet maps to a requirement. Gate 1 (THE-792) is covered by `fix-truth-in-reporting` and
`add-launch-admission`; its rows are listed so no bullet is left without a home.

| Gate acceptance bullet                                                                                                          | Home                                                                                                                                                                                   |
| ------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| THE-793: capture round-trips the dirty, staged, untracked, binary and mode fixture with an identical baseline                   | "Capture is explicit, stable and complete"                                                                                                                                             |
| THE-793: confinement checks reject linked `.git` pointers, alternates, symlinked metadata and shared inodes                     | "A private repository is independent of its source"                                                                                                                                    |
| THE-793: kill, restart and container recreation keep repository and baseline                                                    | "A private workspace is durable and recoverable"                                                                                                                                       |
| THE-793: submodules and LFS refuse or need explicit exclusion                                                                   | "Capture is explicit, stable and complete" (scenario: submodule or LFS content)                                                                                                        |
| THE-793: no host git runs the source repo's executable config during capture                                                    | "Capture runs no source-repository executable configuration"                                                                                                                           |
| THE-793: random UUID workspace, storage split, state machine, opt-in flag                                                       | "A private workspace has a durable identity and split storage"; "A private workspace is durable and recoverable"; "Workspace mode is chosen per workspace, independent of the backend" |
| THE-794: hostile archive and symlink fixtures rejected before review                                                            | "Export is quarantined, bounded and never executes sandbox content"                                                                                                                    |
| THE-794: failpoint tests at every journal, apply and fsync step; unexpected edits during recovery; concurrent and repeated sync | "Sync is journaled and recoverable"; "Sync applies nothing on conflict"                                                                                                                |
| THE-794: every launch route through the handle, no host spawn after a secure rejection                                          | `add-launch-admission`                                                                                                                                                                 |
| THE-795: adapter contract, refusal table, `runtime-lost` with no host fallback, deletion or baseline reset                      | `add-launch-admission`; "A private workspace is durable and recoverable" (runtime loss)                                                                                                |
| THE-795: canaries (home, sibling, git, cache, system paths; direct IP, alternate DNS, IPv6, proxy bypass)                       | `add-launch-admission` scenarios                                                                                                                                                       |
| THE-796: conversion keeps dirty, staged, untracked, binary and mode; old tree kept until confirmation                           | "Existing workspaces convert only by explicit action"                                                                                                                                  |
| THE-796: a fresh install defaults untrusted work to private-copy; an explicit compatibility choice survives upgrade             | "Workspace mode is chosen per workspace, independent of the backend"                                                                                                                   |
| THE-796: docs, help, config example, Home Manager and doctor describe the default                                               | tasks 7.1 to 7.3 (documentation, not a behavioural requirement)                                                                                                                        |
| THE-791 release gates: no lost private data after compute recreation; discard custody; host edits and HEAD or index movement    | "A private workspace is durable and recoverable"; "Discard is a separate, custody-preserving action"; "Sync applies nothing on conflict"                                               |
