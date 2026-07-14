# Tasks — native Windows phase 5 (feature parity)

## 1. Activity dots

- [x] 1.1 `scan_proc` Windows twin via sysinfo (cwd + accumulated CPU time),
      same contract, longest-prefix matching unchanged; sysinfo target-gated
      into thegn-core for `cfg(windows)` only.
- [x] 1.2 Docs updated ("empty off Linux" → Linux /proc, Windows sysinfo).

## 2. Secret-file permissions

- [x] 2.1 `thegn_core::fsperm::{restrict_to_owner, restrict_dir_to_owner}`
      (0600/0700 on unix, owner-only DACL via icacls on Windows) + unix unit
      tests.
- [x] 2.2 Rewired: host `secret.rs` (token file + secrets dir), svc
      `share/mod.rs` credentials, svc `vpn/mod.rs` keys — all best-effort,
      keyring remains the primary store.

## 3. Sandbox backend policy

- [x] 3.1 `pick_backend` declines OCI backends on native Windows with a
      warning naming the same-absolute-path invariant and pointing at WSL2.
- [x] 3.2 Default `backend_chain` gains `"jobobject"` before `"host"`
      (Absent off Windows); config example + default-chain test updated.
- [x] 3.3 Pane-child job resource limits deferred (documented): ConPTY scopes
      the pane tree; revisit after on-machine validation.

## 4. Desktop toasts

- [x] 4.1 `deliver_windows`: WinRT toast via PowerShell (pwsh → powershell,
      explicit — never cmd), null stdio, dispatcher-thread only.

## 5. Validation

- [x] 5.1 Linux: fsperm/sandbox/activity/config test modules green; clippy
      clean; workspace windows-gnu cross-check green, warning-free.
- [ ] 5.2 On-machine (joins the phase-4 checklist): activity dots move under
      a busy pwsh pane; icacls-restricted token file readable by owner only;
      a toast fires on agent-done; podman/docker present → declined with the
      WSL2 message and `jobobject` selected; Git for Windows runs the
      merge_guard pre-merge-commit hook.
