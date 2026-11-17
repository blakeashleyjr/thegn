# Tasks

Implemented across waves W3 and W4 (the spike in W3, the rest in W4); nothing below is started. Tick a task
when its item merges. The owning programme item is named on each line.

## 1. Modes and records

- [ ] 1.1 `WorkspaceMode` type and the `[sandbox.workspace] mode` key (WS00-P1-14 stub; WS11-P3-06c wiring)
- [ ] 1.2 `Feature::PrivateCopy` gate and the opt-in flag (WS00-P1-01; WS11-P3-06c)
- [ ] 1.3 `private_workspaces` table, `worktrees.workspace_mode` and the W4 migration (WS11-P3-06s spike, WS11-P3-06a)
- [ ] 1.4 Storage split and `StorageSupport` admission (WS02-P5-07, WS11-P3-06a)

## 2. Capture and the private repository

- [ ] 2.1 Capture spike (WS11-P3-06s, W3)
- [ ] 2.2 Stable capture of dirty, staged, unstaged and untracked files with the immutable baseline (WS11-P3-06a)
- [ ] 2.3 Inert Git configuration during capture (WS11-P3-06a)
- [ ] 2.4 Fresh private repository by bundle import, confinement checks at admission and reuse (WS11-P3-06b)
- [ ] 2.5 Submodule and LFS refusal or explicit exclusion (WS11-P3-06a)

## 3. Export and review

- [ ] 3.1 Quarantine exporter, hostile path and symlink rejection (WS11-P3-07)
- [ ] 3.2 Patch and manifest review artifact from broker-owned blobs (WS11-P3-07)

## 4. Sync

- [ ] 4.1 B/S/H conflict detection, apply nothing on conflict (WS11-P3-17)
- [ ] 4.2 Journal, ordered apply, fsync, receipts and recovery (WS11-P3-17)
- [ ] 4.3 Failpoints at every journal, apply and fsync step; unexpected edits; concurrent and repeated sync (WS11-P3-17)

## 5. Lifecycle

- [ ] 5.1 State machine and persistence outside the workload (WS11-P3-18)
- [ ] 5.2 Durability across restart, volume loss and compute recreation; `runtime-lost` without host fallback (WS11-P3-18)
- [ ] 5.3 Discard custody and credential expiry (WS11-P3-18)

## 6. Conversion

- [ ] 6.1 Explicit conversion preserving dirty content, old tree kept until confirmation (WS11-P3-19)
- [ ] 6.2 Label existing live workspaces with their exposure (WS11-P5-01)

## 7. Docs and validation

- [ ] 7.1 Help pages, config example and Home Manager description of the default (WS11-P6-05, WS11-P3-11)
- [ ] 7.2 `thegn doctor` shows workspace mode and boundary per launch
- [ ] 7.3 Config and help ratchets for the new key (B10)
- [ ] 7.4 Run `just ci` (includes `openspec validate --all --strict`) once, when the implementation is complete
