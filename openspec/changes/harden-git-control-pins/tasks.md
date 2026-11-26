## 1. Plan

- [x] 1.1 `control_pins` and `pin_target_ok`; every `plan_git_mounts` mode
      appends the pins.
- [x] 1.2 `host_exec_paths` with `HOST_EXECUTED_WORKTREE_PATHS` and
      `existing_host_exec_pins`.

## 2. Lowering

- [x] 2.1 `legacy_git_mounts` pins the control files (remote placements
      included) and fails closed on an unsafe target.
- [x] 2.2 Host-executed pins after the worktree bind for `standard` and above.
- [x] 2.3 systemd `ReadOnlyPaths=` for every pin and the read-only common dir.
- [x] 2.4 Remove `pin_git_control_read_only` from `agent_run`.

## 3. Verification

- [x] 3.1 Unit tests on backend argv: every pin read-only on bwrap, OCI and
      systemd; common dir outside `$HOME`; host-executed pins.
- [x] 3.2 `test/userns-e2e.d/30-git-integrity.sh` (bwrap).
- [ ] 3.3 Remote build and tests green.
